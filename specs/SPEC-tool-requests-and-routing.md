# SPEC-tool-requests-and-routing: Peer tool requests and routing

## Record justification

The contract spans protocol authority and persistence, generic interception,
harness generation validation and registry routing, terminal ownership, session
restore replay, and extension delivery. No single crate or local API documents
the complete commit-before-route invariant.

Configured Provider, Tool, and Core extensions may submit `tool.request`
through generic Emit. The request is a routing intent, not proof that the
harness accepted or started work. Generic admission rejects only an empty call
id or a peer without exact configured-kind authority, then ordinary
interception commits and broadcasts the request before any correlation check,
pending-call mutation, or registry lookup.

The immutable publication context retains the configured name, kind, logical
instance, and live connection generation. After commit, the harness requires
that exact configured generation to remain current. A request parked across
disconnect or replacement remains an observable commit but causes no notice,
bookkeeping, routing, or derived event. An already-known call id likewise
remains committed, but produces only an important notice and cannot alter the
existing owner, route, accounting, or completion tombstone.

For a current unique request, the harness retains the committed payload as
request metadata, establishes pending-call state, and resolves the live tool
registry. Successful extension routing installs the exact terminal-report owner
before publishing harness-sourced `tool.started`. Internal routing correlates
the trusted configured peer's payload agent id to a currently loaded
conversation and has no extension terminal owner. That correlation is
runtime-only and distinct from transcript tool-call ownership. It participates
in pending accounting, wait projection, ephemeral classification, and agent
unload cleanup. Result/error completion publishes ownerless, non-transcript
terminal facts, decrements runtime accounting, clears live correlation, and
retains the completed-call tombstone. Unavailable routing publishes
harness-sourced `tool.rejected`. For an ownerless, non-transcript call it then
publishes protected `tool.error` and `provider.tool_error` in that order and
closes the pending call while retaining its completed-call tombstone.
`tool.started` preserves the
committed request's agent id, arguments, tool identity, and originator;
request-time rejection preserves its tool identity and originator. Later
terminal reports retain their routed producer metadata.

Ownerless, non-transcript calls have no durable provider-terminal journal
authority and remain outside
[DECISION-tool-terminal-publication-transactions](DECISION-tool-terminal-publication-transactions.md).
For journal-backed request rejection, the durable provider terminal instead
commits before renderer projection or pending-call cleanup, as specified by
[SPEC-terminal-tool-reports-and-canonical-outcomes](SPEC-terminal-tool-reports-and-canonical-outcomes.md).

Generic intake preserves `Emit.persist`. A request with `persist=false` is live-only.
A request with `persist=true` enters the session restore stream with the stable
configured publisher name, not its run-local connection id. Replay and
subscribe-time historical delivery expose the stored fact only to matching
selectors and never rerun downstream routing, execution, outcomes, or recovery.
Harness-internal and agent-generated requests retain their existing non-peer
publication path and do not enter the peer consumer.

This flow is governed by
[DECISION-generic-peer-event-emission](DECISION-generic-peer-event-emission.md).
Session restore sequencing follows
[SPEC-tau-harness-event-processing](../crates/tau-harness/specs/SPEC-tau-harness-event-processing.md).
