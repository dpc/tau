# DECISION-agent-watch-acyclic-topology: Keep agent watches acyclic

Authority: confirmed, 2026-07-18, dpc

## Decision

The current-session agent-watch topology must remain a directed acyclic graph.
Self-watch and edges that would create a cycle are invalid.

## Rationale

Watch cycles have no useful product semantics, can amplify watch-derived
interactions, and complicate observer and UI reasoning. This choice excludes
reciprocal observation.

See [SPEC-agent-watch](SPEC-agent-watch.md).
