# GATE-agent-watch-acyclic-topology: Keep agent watches acyclic

## Gate

The current-session agent-watch topology must remain a directed acyclic graph;
self-watch and cycle-forming edges are invalid.

## Justification

The user wants observation to remain one-way and understandable. Cycles provide
no useful product semantics, permit amplification, and make UI and lifecycle
reasoning substantially harder.
