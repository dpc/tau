---
name: tau-self-knowledge-architecture
description: >
  Use this skill when the user asks how Tau is architected, including the harness
  daemon, UI clients, Unix socket connections, sessions, event flow, model
  orchestration, tool routing, or extension processes.
advertise: false
---

# Tau architecture overview

Tau runs as a daemon-centered system.

Typically, a UI process starts (or attaches to) a harness daemon for the current project/session. The UI sends user input and renders streamed events, while the harness owns orchestration.

The harness process:

- manages session state and event flow,
- starts and supervises extension processes,
- routes tool calls to the right extension,
- records session membership, agent transcript, and debug logs,
- builds agent prompts and handles model responses.

Extensions are separate processes connected to the harness. They register tools and capabilities, then handle requests from the harness and return results/events. This separation keeps tool/runtime concerns outside the core harness loop.

Clients connect to the harness over a Unix socket. This allows multiple clients to observe or interact with the same running session while the harness remains the single coordinator.

## Multi-agent watch topology

The harness owns a session-local directed acyclic agent-watch graph. Enabling a
watch fails if the target already reaches the calling watcher; disabling can
remove an existing edge. Watches and their notifications remain local to the
current session. Remote peer addresses are supported by `message`, but are not
watch targets.
