# tau-cli

`tau-cli` is Tau's terminal application layer. It connects to the harness daemon, owns the interactive chat loop, interprets application commands, and renders protocol events through `tau-cli-term`.

## Event flow

The interactive UI has three main flows:

1. the socket reader receives `tau-proto` events from the harness,
2. `EventRenderer` folds those events into terminal-visible state and writes blocks through `tau-cli-term`,
3. the input loop reads high-level prompt events and sends application commands or prompts back to the harness.

The long-term direction is a single UI model/reducer that owns protocol state, with the input loop sending typed commands instead of sharing mutable mirrors of renderer state. Current code still has some shared `Arc<Mutex<_>>` snapshots for completions and prompt editor context; prefer reducing those when touching nearby code.

## Tool UI policy

UI code must render tool calls through generic `ToolUseState`, `ToolUsePayload`, progress counters, and fallback tool displays. Do not add tool-name-specific rendering for ordinary extension tools.

Harness sub-agent activity is rendered from generic events, not
delegation-specific UI paths. `agent.watches_updated` identifies which agents
are observed; structured watch state decides whether an observed agent is in an
outer agent turn, from activating input through terminal response. Individual
provider invocations are inner model rounds, and prompt/provider events are only
a pre-snapshot compatibility fallback. `agent.stats_updated` provides generic
counters and provider response stats provide live response throughput details
for that running turn. A direct target reads `running`; an idle direct target that
watches an active descendant remains visible as `watching -> @descendant`. This
recursive projection is exact over the live watch DAG, keeps one row per direct
target, and contributes unique effective targets to the session-wide bottom
`@N` chip. Merely live, selectable, non-suspended, or idle leaf agents must not
appear as active watched-agent work.

Prompt navigation modes are harness-owned current-session daemon memory:
ordinary agents default to `active`, delegated agents default to `active-auto`,
and any attached UI can request `suspended`, `active`, or `active-auto` with
`/agent suspend`, `/agent resume`, or `/agent auto`. Complete
`agent.stats_updated` snapshots update each UI cache. Selection, transcript,
drafts, and presentation remain UI-local. Explicit overrides are not persisted;
cold restore recomputes defaults from existing provenance. Selecting or
prompting a hidden agent does not change its mode.

The public `tau agent list <session-id>` command reads a directed harness roster
and emits stable headerless TSV; it does not infer membership or navigation from
renderer state. The C-b picker and C-j/C-k navigation ring use the
effective-active rule: `active` agents remain eligible while idle, and
`active-auto` agents are eligible only while running. C-B instead lists every
current live agent, including idle `active-auto` and explicitly suspended
agents. Both pickers retain the independent running/idle column. The overview
remains the input target for starting a new agent.
The shifted default requires enhanced keyboard reporting; legacy terminals may
bind `agent-pick-all` to another distinguishable key.

There is also a narrow temporary action-input redaction exception: `/email auth
google finish ...` command echo and prompt-history entries are redacted because
the pasted Gmail loopback URL contains a one-time OAuth authorization code and
the action schema does not yet provide sensitive-argument metadata. The emitted
`ActionInvoke` still carries the raw argument to the owning extension; this UI
special case should be replaced with schema/protocol metadata when available.

## Threading and shutdown direction

The current implementation has a socket reader thread, renderer path, redraw/timer helpers, and a blocking prompt input loop. Remote disconnect handling is not yet fully unified with prompt input wakeup. Future changes should move toward explicit UI event ownership: daemon disconnect, terminal input, timers, and shutdown should be represented as events that drive one loop or a clearly joined set of owned workers.

## Command paths

Interactive chat, `tau dev send`, and `--prompt-stdin` should share socket/session setup and prompt construction wherever possible. Mode-specific command capabilities are fine, but avoid duplicating protocol handshakes or slash-command parsing in separate paths.
