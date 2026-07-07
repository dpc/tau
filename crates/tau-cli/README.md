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

Harness sub-agent activity is rendered from generic events, not delegation-specific UI paths. `agent.watches_updated` identifies which agents are observed; prompt/provider lifecycle events decide whether an observed agent is currently running a turn; `agent.stats_updated` and `agent.turn_stats_updated` provide counters/details for that running turn. Merely live, selectable, non-suspended, or watched agents must not appear as active watched-agent work or be counted in the bottom active-side-agent `@N` status chip.

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
