# GATE-persistence-and-extension-interface-change-approval: Approve persistence and extension-interface changes

## Gate

Architectural or externally meaningful changes to event logs, journals, or the
harness-extension interface require explicit user or maintainer confirmation of
their exact semantics before implementation. Agents must not choose such
semantics inside unrelated work.

Native Codex standalone compaction may automatically retry a transient failure
only before it accepts semantic compact output. Once it accepts semantic compact
output, any later failure must discard that uncommitted output and terminalize
without automatic retry. An error processed before content from the same event
is accepted remains pre-progress and retryable. Recovery after a post-progress
failure requires a distinct explicit request.

## Justification

The user wants deliberate review of persistence schema, ordering, durability,
replay, recovery, and indexing, plus shared protocol, capability, lifecycle,
tool naming, routing, authority, and trust-boundary changes. Pure bug fixes,
refactors, and editorial corrections remain exempt when they preserve documented
semantics.

The native Codex boundary retains resilience when no semantic work has been
accepted, while preventing automatic duplicate paid work after the provider has
already produced semantic compact output.
