# GATE-persistence-and-extension-interface-change-approval: Approve persistence and extension-interface changes

## Gate

Before implementing significant changes to existing documented event-log,
journal, or harness-extension interface semantics, agents must obtain explicit
user or maintainer confirmation of the exact semantics. Agents must also obtain
confirmation for new semantics with materially consequential or surprising choices,
including data-loss, durability, or replay risks, duplicate paid work, or
expansion of authority or trust boundaries. Agents must not choose these
semantics as part of unrelated work.

Routine fixes, refactors, editorial corrections, and obvious additive
correctness improvements do not require separate confirmation. This includes
recognizing an established provider message phase, preserving available usage
or response identity, adding optional stored fields, and minor turn-handling
adjustments that follow an established provider contract. Touching
stored state, replay, shared types, or turn handling does not by itself make a
change significant. This exemption does not authorize materially changing
existing history interpretation, durability or recovery guarantees, retry or
paid-work policy, or extension authority and trust boundaries.

Agents do not need separate confirmation for an additive user-requested feature
with sensible, low-risk defaults that preserve existing behavior just because
it adds a persistence record or provider-internal interface. For example, adding
a credential record for a requested provider with the existing credential
handling defaults need not trigger approval; changing how the system recovers
existing credentials or who can access them does.

Native Codex standalone compaction may automatically retry a transient failure
only before it accepts semantic compact output. Once it accepts semantic compact
output, any later failure must discard that uncommitted output and terminalize
without automatic retry. An error processed before content from the same event
is accepted remains pre-progress and retryable. Recovery after a post-progress
failure requires a distinct explicit request.

## Justification

The user wants review of significant changes they might otherwise miss, not
approval interruptions for obvious correctness improvements or routine defaults.
Significance depends on the consequences of a change, not on which files,
fields, or state machines it touches. Compatibility analysis and focused
live/replay, restart, cancellation, and accounting tests still apply where
relevant; needing those checks does not itself create an approval requirement.

The native Codex boundary retains resilience when no semantic work has been
accepted, while preventing automatic duplicate paid work after the provider has
already produced semantic compact output.
