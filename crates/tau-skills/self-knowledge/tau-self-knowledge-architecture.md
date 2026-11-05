---
name: tau-self-knowledge-architecture
description: Use when inspecting or explaining Tau architecture, including the harness daemon, UI clients, Unix socket connections, sessions, event flow, model orchestration, tool routing, or extension processes.
advertise: false
---

# Tau architecture overview

The UI starts a harness, attaches to one with `tau attach [SESSION]`, or restores
persisted state with `tau resume [SESSION]`. It sends input and renders streamed
events; the harness daemon owns orchestration.

The harness process:

- manages session state and event flow,
- starts and supervises extension processes,
- routes tool calls to the right extension,
- records session membership, agent transcript, and debug logs,
- builds agent prompts and handles model responses.

Extensions run as separate processes, register tools/capabilities, and return
results/events for harness requests.

Multiple clients can share a session over its Unix socket.

## Multi-agent watch topology

Agent watches form a session-local directed acyclic graph; cycle-forming watches
fail. Remote peer addresses work with `message`, not watches.
