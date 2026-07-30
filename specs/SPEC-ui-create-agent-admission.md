# SPEC-ui-create-agent-admission: UI agent-creation admission

## Record justification

UI agent creation spans shared protocol types, harness creation and prompt admission, interactive and one-shot clients, and transient delivery/replay policy, so no component-local artifact can own the contract coherently.

## Correlated outcome

Every authorized, decoded `ui.create_agent` request carries a nonempty,
connection-lifetime-unique `request_id` of at most 128 UTF-8 bytes. The harness
sends exactly one `ui.create_agent_result` point-to-point to the initiating UI.
The result echoes the request and session ids and either reports the created
agent plus initial-prompt admission state or a stable rejection category,
bounded message, and optional already-created agent id.

The create `request_id` and optional prompt `ctx_id` are separate generated
identities. The former correlates admission only; the latter follows the
materialized initial prompt and its provider chain. A client must not alias or
reinterpret either identity as the other.

Creation succeeds only after `agent.started` commits and the live route exists.
When the request carries an initial prompt, the create result reports `Queued`
once the created agent is durable and the prompt has entered harness-owned
preprocessing. This result does not claim that preprocessing, canonical
submission, or provider execution succeeded.

Later prompt processing remains a separate lifecycle. Preprocessing, submission,
cancellation, or lifecycle teardown before provider materialization publishes a
transient `agent.prompt_failed` terminal carrying the create request id, created
agent id, and prompt `ctx_id`. Once `agent.prompt_created` binds that `ctx_id` to
an agent prompt id, existing prompt termination and provider completion events
carry the rest of the lifecycle. Unsuccessful provider terminals are failures,
not successful one-shot output.

Rejections distinguish invalid correlation, stale session, unavailable role,
invalid metadata, unloaded parent, creation failure, and initial-prompt
failure. Provider and tool failures after admission remain ordinary prompt
lifecycle outcomes.

## Delivery and persistence

The request and directed result are transient operational traffic. Neither
enters agent, session, or restore journals; neither has cold replay,
current-state synthesis, or restart retry. Canonical successful lifecycle,
membership, metadata, transcript, and prompt facts retain their existing
durability.

`--prompt-stdin` waits up to ten seconds for the matching admission result.
Rejection exits unsuccessfully before waiting for provider work. Acceptance
ends only the admission deadline; provider execution remains unbounded.
Subsequent prompt failures, provider updates, completions, and prompt
terminations must match the create request, created agent, prompt `ctx_id`, or
the bound prompt-id chain as applicable, so unrelated user-originated work on
the same daemon cannot complete the one-shot invocation.
