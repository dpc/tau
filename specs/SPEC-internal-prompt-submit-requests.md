# SPEC-internal-prompt-submit-requests: Internal prompt submission

## Record justification

Internal prompt submission spans protocol durability defaults, generic peer
authority/interception, extension activation, exact-generation ownership,
harness prompt validation and queueing, transcript facts, and the timer producer.
No component-local artifact can describe the complete contract.

This specification implements the `extension.internal_prompt_submit_request`
part of the request row in
[SPEC-peer-event-publication](SPEC-peer-event-publication.md).
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

The committed request names a loaded agent, internal model-classified text, and an
optional submitter correlation `ctx_id`. It also carries optional typed
activation provenance: absent or `internal_prompt` preserves ordinary internal
prompt behavior, while `timer` classifies the resulting content-free queued
activation as a timer. This field grants no authority beyond the existing
configured-local-extension admission. The downstream consumer preserves the
existing loaded-agent and session-route validation. An invalid target remains
a committed observation, produces the existing harness rejection diagnostic,
and creates no prompt fact.

An accepted request enters the ordinary per-agent prompt queue as internal text.
It has no user-message class and does not update latest-user-interaction
metadata. The harness publishes canonical `agent.prompt_submitted` or
`agent.prompt_steered` facts through the existing prompt path; steering retains
the request's `ctx_id`. The harness stamps those facts with the authenticated
configured extension name as `PromptSubmissionSource::Extension`; it never
trusts extension-supplied provenance. CLI presentation renders that source once
as an attributed message block, while typed `HarnessInternal` prompt facts use
the default-off `show_internal_prompts` diagnostic setting. Legacy provenance
remains hidden, and the source changes neither model delivery nor replay.

The request is operational traffic, not an extension-lifecycle activation
declaration. Its typed provenance only classifies the harness-authored
`agent.activation_queued` observation. Pre-Ready requests remain behind the
extension activation barrier and execute only after the source reaches Ready
and earlier ordered activation work settles.

## Persistence and first-party producer

The raw request defaults to `persist=false` and is unconditionally excluded from semantic
agent, session, and restore journals for either caller-supplied
`Emit.persist` value. It has no cold replay or current-state synthesis.
Harness-owned prompt facts keep their existing transcript classifications.
`tau-ext-utils` sends timer wake requests with `persist=false` and explicit
`timer` provenance. Replay does not resubmit a request or alter timer recovery.

The configured-local-extension boundary is documented in
[`SECURITY.md`](../SECURITY.md). Unrelated authority-matrix rows remain outside
this specification.
