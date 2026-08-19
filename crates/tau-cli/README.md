# tau-cli

`tau session stats --session <id>` reads the session membership journal and only
the journals of agents that ever belonged to that session. It emits versioned TOON
by default with exact turn, tool, token, model/effort, and captured estimated-cost
counts. Estimated API costs appear as `estimated_api_cost_dollars`, rounded to the
nearest microdollar for readable output; internal aggregation retains exact
picodollars. `complete: false` plus `missing_data` identifies pre-contract or
missing facts; corrupt journals fail the command instead of producing partial
totals.

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
are observed; current-session structured work status keeps a watched row
visible while it is unreported, working, waiting, blocked, or unknown, and removes it
only after done. The complete `agent.stats_updated` detailed activity decides its turn emoji,
while binary runtime remains navigation authority. Individual provider invocations are
inner model rounds, and prompt/provider events are only a pre-stats
compatibility fallback. `agent.stats_updated` also provides generic counters
and provider response stats provide live response throughput details for that
running turn. An idle watched target retains its status row, and one that
watches an active descendant adds `watching -> @descendant`. This recursive
projection is exact over the live watch graph. It shows the full visible
deduplicated closure through eight rows, then falls back to every direct watch
without truncating that direct set. Indirect rows identify their chosen
predecessor as `via @parent`. Activity projection separately contributes unique
effective targets to the session-wide bottom `@N` chip. Merely live, selectable,
non-suspended, or idle leaf agents do not appear as active watched-agent work or
contribute to that count.

Prompt navigation modes are harness-owned current-session daemon memory:
ordinary agents default to `active`, delegated agents default to `active-auto`,
and any attached UI can request `suspended`, `active`, or `active-auto` with
`:agent suspend`, `:agent resume`, or `:agent auto`. Complete
`agent.stats_updated` snapshots update each UI cache. Selection, transcript,
drafts, and presentation remain UI-local. Explicit overrides are not persisted;
cold restore recomputes defaults from existing provenance. Selecting, picking, or
focusing a hidden agent alone does not change its mode. A successfully admitted
visible user prompt to an existing target is an implicit absolute `active` write;
the harness publishes the resulting complete stats before queue or dispatch.

The public `tau agent list <session-id>` command reads a directed harness roster
and emits stable headerless TSV; it does not infer membership or navigation from
renderer state. The C-b picker, `:pick-agent`, and C-j/C-k navigation ring use
the effective-active rule: `active` agents remain eligible while idle, and
`active-auto` agents are eligible only while running. `:pick-agent-all` instead
lists every current live agent, including idle `active-auto` and explicitly
suspended agents. Both pickers render work status and current-turn state as
compact emoji; lifecycle and role remain
available from `tau agent list` but are omitted from picker presentation. The
overview remains the input target for starting a new agent. The underlying
picker actions remain configurable, and the all-agent action has no default
key binding.

`tau agent trace <agent-id>` operates offline and projects from a stable,
validated snapshot of existing durable agent journals. It defaults to the compact
`agent-tools-toon` semantic timeline in lite mode, which keeps exact content
sizes and at most 4 KiB of each assistant/reasoning/message text and terminal
output rather than complete forensic evidence. Explicit `tau-jsonl` is the
complete native
artifact and preserves every persisted event and its journal-local ordering.
`otlp-json` is a lossy OpenTelemetry/OpenInference visualization adapter: it
derives spans only from durable IDs and journal wall-clock timestamps, while
retaining every raw journal occurrence as a span event.
`agent-tools-toon` and `agent-tools-jsonl` provide compact relative/absolute
journal timelines over provider-declared calls, assistant prose/reasoning,
explicit directional messages, activations, and typed causal relationships.
`--mode lite` is the default and reports complete text/output byte/line counts,
bounded content, and explicit completeness; `--mode full` includes complete
semantic text and rendered output.
`agent-performance-jsonl` is always content-free and reports response-local
token/cache/cost evidence plus qualified journal recorded-at wall intervals and
per-agent summaries. `--mode full` is invalid for this format.
See [`docs/agent-trace.md`](../../docs/agent-trace.md) for the output contracts,
failure behavior, and sensitive-data warning.

`tau session list` prints one escaped row per distinct current session id
reported by responsive local harnesses. Runtime paths only locate socket
candidates; each daemon reports its in-memory current session and immutable
canonical startup project root through a directed local control RPC, and
persisted session directories never add rows.
Backslash, tab, newline, carriage return, and other control characters use the
same `\\`, `\t`, `\n`, `\r`, and `\u{hex}` escaping as agent-list fields.
This makes records line- and ANSI-control-safe; it does not normalize general
Unicode format characters.

`tau session list --dir DIR` canonicalizes an existing directory and returns
only exact project-root matches. `--json` emits one array whose records contain
required `session_id` and `project_root` strings; empty results are `[]`, and
duplicate records are retained when multiple responsive harnesses report the
same identity.

Relative `--dir` values resolve from the caller's current directory. Missing,
inaccessible, and non-directory values are CLI errors with exit status 2.
Zero, one, and multiple matches are successful complete snapshots, and a closed
output pipe is also success. Other discovery, probe, serialization, or output
failures return nonzero. Discovery, probe, and serialization failures occur
before stdout is touched; a stdout write failure can leave a written prefix
because arbitrary output streams cannot be rolled back. The command only
inspects runtime candidates and does not create or clean up state.

There is also a narrow temporary action-input redaction exception:
content-enabled prompt drafts represent a recognizable `:email auth google
finish ...` buffer as exactly `:email auth google finish <redacted>` during
composition. After submission, every history/editor presentation uses that same
fixed line. The pasted Gmail loopback URL contains a one-time OAuth authorization
code and the action schema does not yet provide sensitive-argument metadata.
Only the active editor, immediate routing stack, and exact owning action
extension retain the raw line; this UI special case should be replaced with
schema/protocol metadata when available.

## Threading and shutdown direction

The current implementation has a socket reader thread, renderer path, redraw/timer helpers, and a blocking prompt input loop. Remote disconnect handling is not yet fully unified with prompt input wakeup. Future changes should move toward explicit UI event ownership: daemon disconnect, terminal input, timers, and shutdown should be represented as events that drive one loop or a clearly joined set of owned workers.

## Command paths

Interactive chat, `tau dev send`, and `--prompt-stdin` should share socket/session setup and prompt construction wherever possible. Mode-specific command capabilities are fine, but avoid duplicating protocol handshakes or command parsing in separate paths.
Bare `:tree` is a one-shot exception to fire-and-forget `tau dev send`: it
waits for and prints the harness's single requester-directed multiline notice.
