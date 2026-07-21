# SPEC-agent-metadata-requests-and-canonical-facts: Metadata mutation flow

## Status

This specification implements the request-to-canonical-fact portion of the
agent metadata row in
[DECISION-generic-peer-event-emission](DECISION-generic-peer-event-emission.md).
Validation failures retain the established behavior: the committed request has
no canonical successor. A typed or directed rejection interface is not yet
specified and remains outside this migration slice.

## Record justification

This contract spans protocol names and defaults, configured-extension and UI
authority, generic interception, harness validation and semantic persistence,
replay synthesis, and the core-shell request/commit-echo lifecycle. No
component-local record can define the complete boundary.

## Authority and ordering

Every authenticated configured extension entry kind, including configured
Core, may author `agent.metadata_set_request` and
`agent.metadata_unset_request` without a capability. An attached socket UI has
the same authority. Unconfigured, disconnected, dedicated external-message,
non-UI socket, and other peers have no authority. Peers cannot author canonical
`agent.metadata_set` or `agent.metadata_unset` facts.

Generic Emit captures the configured extension's exact run-local connection,
stable configured name, instance id, and kind before ordinary same-name
interception. UI requests retain their run-local socket connection id. After
request commit and broadcast, the consumer revalidates the complete configured
extension generation or the still-attached UI connection. A stale request may
remain observable but cannot mutate metadata.

The downstream consumer applies the existing agent target, reserved-key, key
size, mutation-id size, CBOR encoding, and value-size validation. A valid
request causes a distinct harness-sourced canonical fact to traverse ordinary
interception, durable folding, and broadcast. A store failure prevents the
canonical echo. Request and canonical publication are intentionally separate
commits.

## Interception and correlation

Interception may drop or replace an uncorrelated request under the same event
name. A set request carrying `mutation_id` is must-pass. Replacement preserves
its agent id, key, mutation id, and inheritance flag while allowing the value
to change. Request replacements commit before downstream validation and may
therefore have no canonical successor. Interception alone cannot drop or
retarget a correlated request. Downstream validation, stale-source rejection,
or store failure may still produce no canonical echo while rejection outcomes
remain outside this migration slice. These are the existing canonical-set
identity protections applied at the request boundary.

Canonical metadata facts retain their existing interception behavior. This
slice does not make them generally immutable or must-pass.

## Persistence, replay, and producers

Requests default transient and never enter semantic agent, session, or restore
journals for either caller-supplied transient value. They are operational
observations and never rerun after restart. Canonical facts remain durable,
extension-visible state; replay synthesizes the latest folded values before
`session.agent_loaded` and removes live mutation ids.

`tau-ext-shell` sends explicit transient set requests and completes setters
only after receiving the correlated canonical commit. Its subscriptions,
runtime cache, restored fixtures, and inherited metadata continue to consume
canonical facts. Internal harness metadata publication remains canonical.

This contract does not change extension kind assignment, Tool-versus-Action/Core
semantics, the general publisher delivery envelope, or any other authority row.
