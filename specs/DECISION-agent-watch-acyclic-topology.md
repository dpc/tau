# DECISION-agent-watch-acyclic-topology: Keep agent watches acyclic

Authority: confirmed, 2026-07-18, dpc

## Decision

The accepted current-session agent-watch topology is a directed acyclic graph.
Self-watch is invalid. A genuinely new `watcher -> watched` enable is rejected
without watch-state mutation when `watched` already reaches `watcher`.
Re-enabling an identical edge preserves its established snapshot and
subscription semantics, and disabling bypasses cycle analysis.

Reachability validation and mutation execute synchronously in the same
serialized harness event-loop operation. Notifications remain direct and
session-local; acyclic topology does not make notification fanout transitive.

## Rationale and tradeoffs

Watch cycles have no useful product semantics, can amplify watch-derived
interactions, and complicate observer and UI reasoning. The tradeoffs are that
reciprocal observation is unavailable and each genuinely new edge may require a
linear graph traversal.

Allowing cycles with SCC or feedback suppression, automatically removing or
reorienting an existing edge, and capped or otherwise inexact traversal were
rejected.

See [SPEC-agent-watch](SPEC-agent-watch.md).
