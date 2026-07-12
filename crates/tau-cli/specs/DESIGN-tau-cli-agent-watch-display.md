# DESIGN-tau-cli-agent-watch-display: Agent names and watcher display

Status: confirmed, 2026-07-07, dpc

Terminal UI agent names render the human display label for an agent; they are
not a place to encode parent lineage, delegation topology, or who is observing
the agent. When the UI needs observation context, `agent.watches_updated` is the
source of truth.

The CLI may keep session-scoped forward and reverse watcher caches derived from
those complete watcher snapshots so it can render current-agent status such as
`watched by: <agent_id>` or a truncated multi-watcher form. These caches must be
cleared on session reset and rebuilt from live or replayed watch snapshots; they
must not mutate display names or durable transcript state.

Two agent-state concepts are intentionally distinct:

- `active` / `active-auto` / `suspended` is per-UI, memory-only navigation
  state. `active` is always offered, `active-auto` is offered only while
  `agent.stats_updated.runtime_state` is `running`, and `suspended` is never
  offered. It does not affect loading, addressability, or prompt/message
  delivery. Delegated agents default to `active-auto`; ordinary agents default
  to `active`.
- `running` / `waiting` is execution state. It controls whether an agent is
  currently processing an outer agent turn after receiving an activating input
  and before its final response or termination returns control to the prompting
  user or agent. An agent turn includes every inner model round and intervening
  tool round; a provider response that requests tools does not end it.

Watched-agent `watching` blocks and the bottom status `@N` side-agent count use
the running/waiting concept, not the navigation concept. Watches identify the
observed agents, and structured watched-agent turn state is authoritative once
received. Prompt/provider lifecycle remains only a compatibility and catch-up
fallback before that snapshot; those inner model-round events must not remove
or recreate an indicator during one running agent turn. Agent stats and provider
response stats only add counters/details to already-running indicators. A
watched or non-suspended agent that is waiting for a future prompt/message must
not be rendered as running. The `@N` status chip counts running side agents and
excludes the currently visible agent, which is already named on the left side
of the status line.

When multiple watched agents are running at once, their `watching` blocks are
ordered by stable agent id. This keeps redraws and independent prompt/stat
events from making visually similar live blocks swap places.
