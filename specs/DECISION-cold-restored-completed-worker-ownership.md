# DECISION-cold-restored-completed-worker-ownership: Cold-restored completed worker ownership

Authority: confirmed, 2026-07-21, dpc

A completed durable worker created through harness-owned `agent_start` or the
typed start-agent request path restores as an ordinary loaded, idle, addressable
conversation. The harness must not reconstruct ownership by the completed
transient request from durable prompt or response provenance. Historical
originators, creation ancestry, task metadata, and tool-call transcript facts
remain unchanged; only the run-local request owner, connection, correlation,
and result/teardown responsibility end with the completed request.

The immutable `agent.started` creation fact remains the cold-recovery authority
for ancestry and default navigation classification. A restored completed worker
whose creation fact names a parent receives the delegated `active_auto` default
without reviving the completed request. A fresh user turn uses ordinary runtime
ownership, emits neither another `agent.start_result` nor
`session.agent_unloaded`, and leaves the worker loaded and addressable after its
terminal response.

This choice preserves the existing journals and protocol: it adds no durable
fact, schema, replay, or request recovery. It does not decide recovery behavior
for a start-agent request interrupted before completion. Warm completion and
cold restoration converge only for completed work.

This decision refines the transient request contract in
[SPEC-start-agent-requests](SPEC-start-agent-requests.md) and the restoration
and navigation behavior in
[SPEC-tau-harness-session-state](../crates/tau-harness/specs/SPEC-tau-harness-session-state.md).
It applies the shared navigation policy in
[DECISION-harness-owned-agent-navigation-modes](DECISION-harness-owned-agent-navigation-modes.md).
It follows the approval requirement in
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md).
