# GATE-persistence-and-extension-interface-change-approval: Approve persistence and extension-interface changes

## Gate

Architectural or externally meaningful changes to event logs, journals, or the
harness-extension interface require explicit user or maintainer confirmation of
their exact semantics before implementation. Agents must not choose such
semantics inside unrelated work.

## Justification

The user wants deliberate review of persistence schema, ordering, durability,
replay, recovery, and indexing, plus shared protocol, capability, lifecycle,
tool naming, routing, authority, and trust-boundary changes. Pure bug fixes,
refactors, and editorial corrections remain exempt when they preserve documented
semantics.
