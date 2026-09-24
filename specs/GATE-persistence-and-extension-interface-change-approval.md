# GATE-persistence-and-extension-interface-change-approval: Approve persistence and extension-interface changes

## Gate

Changes to existing documented event-log, journal, or harness-extension
interface semantics require explicit user or maintainer confirmation of their
exact semantics before implementation. So do new semantics with materially
consequential or surprising choices, including data-loss, durability, or replay
risks, duplicate paid work, or expansion of authority or trust boundaries.
Agents must not choose these semantics inside unrelated work.

An additive user-requested feature with sensible, low-risk defaults that
preserve existing behavior does not require separate confirmation merely
because it adds a persistence record or provider-internal interface. For
example, adding a credential record for a requested provider with the existing
credential handling defaults need not trigger approval; changing how existing
credentials are recovered or who can access them does.

Native Codex standalone compaction may automatically retry a transient failure
only before it accepts semantic compact output. Once it accepts semantic compact
output, any later failure must discard that uncommitted output and terminalize
without automatic retry. An error processed before content from the same event
is accepted remains pre-progress and retryable. Recovery after a post-progress
failure requires a distinct explicit request.

## Justification

The user wants to catch harmful changes they might otherwise miss, without
rubber-stamping routine defaults for requested new features. Review remains
deliberate for changes to existing persistence schema, ordering, durability,
replay, recovery, or indexing and shared protocol, capability, lifecycle, tool
naming, routing, authority, or trust boundaries. Pure bug fixes, refactors, and
editorial corrections remain exempt when they preserve documented semantics.

The native Codex boundary retains resilience when no semantic work has been
accepted, while preventing automatic duplicate paid work after the provider has
already produced semantic compact output.
