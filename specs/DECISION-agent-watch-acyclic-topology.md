# DECISION-agent-watch-acyclic-topology: Keep agent watches acyclic

Authority: confirmed, 2026-07-18, dpc

The accepted current-session agent-watch topology is a directed acyclic graph.
Self-watch is invalid. A genuinely new `watcher -> watched` enable is rejected
without watch-state mutation when `watched` already reaches `watcher`.

Reachability validation and mutation execute synchronously in the same
serialized harness event-loop operation. Notifications remain direct and
session-local; acyclic topology does not make notification fanout transitive.

Watch cycles have no useful product semantics, can amplify watch-derived
interactions, and complicate observer and UI reasoning. The tradeoffs are that
reciprocal observation is unavailable and each genuinely new edge may require a
linear graph traversal.

See [SPEC-agent-watch](SPEC-agent-watch.md).
