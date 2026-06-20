# Global Agents Rework

Make agents (aka conversations, aka threads) global, durable objects that can be
loaded into sessions.

## Terms

- **Agent**: a resumable LLM conversation/thread. It owns the transcript, tree,
  branch state, prompt history, tool round history, compaction state, cwd, role
  metadata, and debugging metadata needed to continue the conversation later.
- **Session**: an agent-membership container. It owns which global agents are
  loaded into that session. It does not persist workspace/cwd metadata yet
  because one session may span multiple workspaces.
- **Runtime event**: a fact committed by the harness while Tau is running. We do
  not need a separate durable runtime WAL for the first version. Durable session
  and agent logs are persisted views of committed runtime facts.

## Agent load replay

Agent logs are independently ordered by their `events.cbor` file order. When an
agent is loaded into a session, the harness replays that agent's durable events
once, in file order, to initialize the UI transcript for that agent.

If multiple agents are loaded, each loaded agent gets its own replay stream.
The session does not store or replay a duplicated mixed transcript. Live events
after the load are delivered normally by the running harness.

Durable records may still carry wall-clock `recorded_at` metadata for display
and diagnostics.

## Agent persistence

Agents live under the Tau state directory, e.g.:

```text
~/.local/state/tau/agents/<agent_id>/
  events.cbor
  meta.json
  lock
```

The agent log is the canonical conversation transcript. It stores agent-scoped
facts such as:

- user prompts and hidden/internal prompts that affect the model input;
- assistant responses/output items, including provider reasoning/opaque items;
- tool calls and terminal tool results/errors/cancellations that belong to the
  agent's transcript;
- compaction facts;
- tree/branch state;
- agent metadata changes that affect future continuation.

Agent events do **not** carry `session_id`. An agent is global and is not owned
by whichever session happens to have it loaded.

Agent metadata includes at least:

- original prompt / creation reason;
- created and last-updated timestamps;
- immutable starting cwd / creation root;
- extension-visible durable metadata, including inheritable keys copied to child agents;
- role/model/debug metadata such as git hash where useful.

The sidecar starting cwd records where the agent began. The current shell cwd is
extension-owned durable metadata (`ext_<extension-instance>_cwd`, for example
`ext_core-shell_cwd`) folded from committed metadata events. Shell/file tools use
that remembered cwd for relative paths; explicit cwd arguments and the `cd` tool
update the metadata so resumes and child agents see the same execution context.

While an agent is loaded for write in a session, the harness holds the agent's
exclusive filesystem lock. Locking prevents multiple harnesses from concurrently
advancing the same agent.

## Session persistence

Sessions remain under the Tau state directory, e.g.:

```text
~/.local/state/tau/sessions/<session_id>/
  events.cbor
  meta.json
  lock
```

The session log is the canonical agent-membership journal, not the canonical
transcript. It stores only the facts needed for the harness to determine which
global agents should be loaded when the session is resumed:

```text
session.agent_loaded { agent_id }
session.agent_unloaded { agent_id }
```

The harness folds this journal on resume: loaded agents are added to the current
set, unloaded agents are removed. Historical load/unload events are not replayed
to the UI as transcript or user-visible history. On reconnect/resume, the
harness announces the current loaded-agent snapshot and replays each currently
loaded agent's durable events once.

Do **not** persist agent active/suspended state in the session log. Running,
idle, delegated, and completed states are runtime/transcript-derived. UI prompt
targeting preferences that are not derivable should remain runtime/UI-local
until there is a concrete need to persist them.

The session log does **not** duplicate normal agent transcript events and does
not persist one `agent_event` reference for every agent event.

## UI reconnect and recovery

When a UI reconnects or a new UI attaches:

1. The harness folds the session membership journal to compute the current
   loaded-agent set.
2. The harness loads/locks those agents and announces a current loaded-agent
   snapshot to the UI.
3. For each loaded agent, the harness replays that agent's durable events once,
   in agent-log order, so the UI can rebuild the agent transcript.
4. Live runtime snapshots fill in currently queued prompts, in-flight tools,
   streaming progress, and other non-durable status for an already-running
   harness.

Cold restart can repair or mark in-flight work as interrupted, as current
session restore already does for lost foreground/background tool calls.

## Prompt assembly

Once global agents exist, prompt assembly reads from the agent log/tree. The
session log is not the source of truth for continuing an agent.

This avoids split authority:

- agent log answers: "what is this agent's conversation and current branch?";
- session log answers: "which agents should this session load?".

## Debug trace

For durable sessions, `events.jsonl` remains an append-only debugging trace. It
includes committed harness runtime events, including some events that are not
persisted into session or agent `events.cbor` streams. Ephemeral-session mode
does not create this file, and ephemeral-agent transcript/debug entries are
suppressed so the debug trace does not become a parallel durable transcript. The
file may therefore duplicate facts that also appear in durable semantic logs, but
it is not a complete audit stream for memory-only agents.

`events.jsonl` is not authoritative state: it is not used for prompt assembly,
session recovery, or agent recovery. The durable semantic sources remain the
agent `events.cbor` logs for conversation continuity and the session
`events.cbor` membership journal for deciding which agents to load.

## Cross-agent messages

The harness-owned message tool is the main cross-agent case and needs special handling.

A message is one logical tool action with a stable `message_id`. It is projected
as explicit runtime events into affected agent logs:

- `agent.message_sent` records an outbound message item in the sender agent log;
- `agent.message_received` records an inbound message item in the recipient
  agent log when the recipient is an agent;
- if the recipient is `user`, there is no recipient agent log entry; the sender
  log outbound item plus live UI handling is enough to show the message to the
  user.

The inbound recipient projection is the durable receipt fact; the harness also
queues a hidden `agent.prompt_submitted` prompt so prompt submission remains the
single source of model-visible input. The outbound sender item lets the sender
remember what it sent. The small message payload is intentionally duplicated
between sender and recipient logs because recipient prompt assembly must not
depend on reading the sender's log.

If a UI displays both sender and recipient transcripts, it can use the shared
`message_id` to correlate or deduplicate the two projections for display.

## Flag-day migration plan

This migration should be implemented as one semantic cutover. We do not need to
read, convert, or preserve old session transcript logs. Intermediate compile
checkpoints are useful while working, but the repository should not carry a
compatibility mode, feature flag, or dual-write path for the old model.

Cutover rules:

- old session `events.cbor` transcript history may be ignored or rejected with a
  clear message;
- new code writes only the new session membership journal and global agent logs;
- normal transcript facts are never written to the session log and the session
  log does not contain per-agent event references. Semantic duplication is
  limited to intentional projections such as `agent.message_sent` and
  `agent.message_received`; durable `events.jsonl` is a debug mirror for
  durable-agent runtime events and intentionally omits ephemeral-agent content;
- tests should assert absence of old transcript facts in the session log.

A good crate-by-crate order is:

1. **`crates/tau-proto` — protocol shape first.**
   - Add an `AgentId` newtype alongside `SessionId`.
   - Add durable session membership events:
     `session.agent_loaded { agent_id }` and
     `session.agent_unloaded { agent_id }`.
   - Use split `agent.message_sent` / `agent.message_received` projections with
     a shared stable `message_id` and agent ids for sender/recipient routing.
   - Remove `session.agent_state_changed` as durable state. Keep only whatever UI
     request events are still needed to load/unload/create agents.
   - Convert transcript-affecting events to agent ownership. For a flag day this
     can be clean: prompt-created/user-message/compaction/provider-output/tool-
     terminal events should carry `agent_id` when they need routing on the live
     bus, and persisted agent records rely on their containing agent directory as
     the source of ownership.
   - Rename ids/events where the old name would keep the wrong model alive
     (`SessionPromptId` can become `PromptId` or `AgentPromptId`; avoid keeping
     optional `target_agent_id` compatibility fields).

2. **`crates/tau-core` — split durable stores.**
   - Extract the current transcript tree machinery into an agent-owned model
     (`AgentTree`, agent entries, background-tool reconstruction, branch helpers).
   - Add `AgentStore` rooted at `<state>/agents/<agent_id>/` with
     `events.cbor`, `meta.json`, and an exclusive `lock`.
   - Slim `SessionStore` to a membership journal and a small folded
     `SessionMembership` view. It should no longer own a transcript tree, prompt
     branch, tool round state, or agent lifecycle state.
   - Keep validation strict: session stores accept only membership events; agent
     stores accept only agent-scoped transcript/metadata events.
   - Prefer copying/renaming the existing append-only log code first. Add a
     shared generic log helper only if the duplication remains large after the
     split.

3. **`crates/tau-harness` — move runtime authority to agents.**
   - Replace session-transcript state with `session -> loaded AgentId set` and
     `AgentId -> agent runtime/conversation state` maps.
   - On session start/resume, fold the session membership journal, load and lock
     each current agent, announce the loaded-agent snapshot, then replay each
     loaded agent's durable events once in agent-log order.
   - If a resumed session has no loaded agents, wait for the first prompt or an
     explicit UI create-agent request to mint and load a durable user agent.
   - Route persistence from the runtime event log: membership facts go to the
     session store; transcript/tool/compaction/metadata facts go to the relevant
     agent store; cross-agent messages write the sender/recipient projections;
     durable `events.jsonl` records committed runtime events except
     ephemeral-agent content.
   - Prompt assembly reads only the target agent's tree/log. The session log is
     consulted only to decide which agents are loaded.
   - Change prompt/tool bookkeeping from session branch cursors to agent branch
     cursors: `prompt_id -> agent_id`, `tool_call_id -> agent_id`, delegate query
     ids -> agent ids, etc.
   - Remove persisted active/suspended state. Running/idle/delegated/completed is
     derived from live runtime plus agent transcript repair after restart.
   - Move cwd/tool execution root to agent metadata and pass it as harness tool
     execution context. Explicit tool `cwd` arguments remain overrides.
   - Rework late UI replay by reusing the current session-replay shape with a new
     source: durable agent events, followed by live runtime snapshots.

4. **Provider and extension crates — follow the new routing fields.**
   - Provider backends should key responses by prompt id and let the harness map
     prompt id back to agent id.
   - Shell/filesystem tools should receive the harness-provided execution context
     so they can default to the agent cwd without model-authored `cwd` fields.
   - Extensions that currently treat `SessionStarted` as the source of durable
     conversation context should treat it as runtime setup only. Extension
     context can still be live session setup, but it should not be mistaken for
     the agent transcript source of truth.

5. **`crates/tau-cli`, terminal crates, and UI tests — display the new model.**
   - Maintain loaded-agent UI state from the current loaded-agent snapshot and
     live load/unload events, not from historical replayed lifecycle events.
   - Build transcripts from per-agent replay streams and live agent events.
   - Remove renderer behavior that depends on `session.agent_state_changed`.
   - Update `/agent` commands to create/load/unload global agents rather than
     mutate session-scoped active/suspended state.

6. **Tests and docs — lock in the cutover.**
   - Core tests: session membership folding; agent tree replay; strict store
     validation; lock behavior.
   - Harness tests: cold resume loads current agents; each loaded agent replays
     once; prompt assembly ignores the session log; transcript events are absent
     from session `events.cbor`; interrupted tools are repaired from agent logs.
   - Cross-agent tests: sender outbound and recipient inbound projections share a
     `message_id`; user-recipient messages do not require a recipient agent log.
   - Debug-log tests: `events.jsonl` receives durable-agent runtime events,
     including runtime-only events not present in semantic CBOR logs, while
     omitting ephemeral-agent content.
   - CLI/UI tests: reconnect renders loaded agents and transcripts from agent
     replay without historical load/unload spam.
   - Update docs and remove old comments that describe sessions as transcript
     owners.

## Consequences

- Agent logs are required to reconstruct conversations. A session directory by
  itself is no longer a complete transcript archive.
- Agent transcript initialization happens by replaying each loaded agent's log
  once on load; per-agent transcript order remains exact.
- Session replay no longer needs to store a duplicated mixed transcript or
  replay historical load/unload events as fresh UI history.
- Data duplication in semantic logs is limited to intentional projections such
  as cross-agent messages, not every transcript fact. `events.jsonl` is allowed
  to duplicate everything for debugging.
- The model keeps the two important authorities separate: global conversation
  continuity belongs to agents; session membership belongs to sessions.
