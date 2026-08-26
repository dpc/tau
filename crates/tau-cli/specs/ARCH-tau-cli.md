# ARCH-tau-cli: tau-cli architecture

The CLI consumes harness-validated provider-neutral quota snapshots and applies
the fixed weekly pacing classifier from
[SPEC-provider-quota-pacing](../../../specs/SPEC-provider-quota-pacing.md).
It selects only an exact current/viewed `ModelId` binding, preserves provider
timestamps during catch-up, keeps per-cycle hysteresis locally, and renders the
accessible compact `Q-`, `Q=`, `Q+`, `Q!`, or `Q?` status chip.
The durable, content-free `agent.prompt_started` fact supplies the selected
agent's model for live lifecycle tracking; historical catch-up excludes it.
Provider quota current-state is capability evidence for neutral `Q?`;
only a fresh exact binding and trustworthy weekly timing permit colored pacing.
Capability lasts for the running harness: a replayed empty snapshot after
provider clear keeps live and late clients converged on neutral unknown.
The lifecycle split is governed by
[SPEC-compact-prompt-materialization-authority](../../../specs/SPEC-compact-prompt-materialization-authority.md).

Terminal bells and OSC user-variable writes are live-only side effects. The CLI
requests their event names only in its live selector set and independently drops
replay-marked terminal-output deliveries before rendering. See
[SPEC-terminal-output-side-effect-events](../../../specs/SPEC-terminal-output-side-effect-events.md).

The one-shot `--prompt-stdin` sink chooses presentation policy independently
for inherited stdout and stderr. When stdout is a terminal, it applies the
terminal-body sanitizer only to dynamic answer text. When stderr is a terminal,
it applies the same sanitizer only to dynamic reasoning, role, rejection,
prompt-failure, and provider-failure bodies. Nonterminal descriptors retain the
captured semantic UTF-8 bytes inside the existing headers, prefixes, separators,
and trailing newlines. This presentation boundary does not modify stdin,
canonical events, protocol traffic, transcripts, journals, or persistence.

The terminal UI executes trusted local configuration and environment-derived
commands, including key-binding shell snippets, completion commands, `$EDITOR`,
and `$VISUAL`. Treat `cli.yaml`, inherited environment variables, and PATH as
local code execution inputs rather than untrusted data.

The prompt's right-side context renders `<cwd> <&session-id>` as one
`prompt.cwd`-styled unit. If prompt input needs that space, terminal overflow
hides the complete unit. The bottom status line identifies the selected role or
agent but does not repeat the session id. Its mandatory priority-zero selected
agent identity is the single `<work-emoji><turn-emoji> @agent` unit. Other
independently hideable elements use ascending importance bands: context `10`,
tool and active side-agent activity `20`, agent description,
selected-agent task title, and model adjustments `30`, watchers `40`, runtime
estimated API cost and weekly quota `50`, UI-I/O
diagnostics `60`, and the redraw
counter `70`. Larger priorities disappear first at narrow widths; equal
priorities disappear in reverse visual declaration order. Retained elements
keep their normal left/right placement and spacing. If identity itself cannot
fit, the status line stays empty rather than wrapping or clipping it.
The reusable fitting and grouping behavior comes from
[ARCH-tau-term-screen](../../tau-term-screen/specs/ARCH-tau-term-screen.md).
Authoritative `session.started` events reconcile the displayed context and the
input loop's routing session, including transitions initiated by another
attached UI.

Tool-call headers use the same adaptive single-row layout and preserve their
existing visual field order. Their importance bands are tool identity `0`,
exact result/lifecycle status `10`, error details `20`, arguments `30`, agent id
`40`, mode `50`, range `60`, diff or progress counters `70`, generic
informational chips `80`, and duration `90`. Identity truncates within `4..=32`
columns, error details and arguments within `5..=48`, agent ids within
`5..=32`, mode within `3..=16`, and range within `5..=32`; all use the exact
middle marker `┄`. Status and numeric/informational chips remain atomic. Tool
identity and every present status-band item form an essential set, so terminals
too narrow for both show no ambiguous header rather than hiding whether a call
succeeded or failed. Expanded payload and diff bodies remain ordinary detail
rows below the one-row header and hide with an essential header that cannot
fit, while compact and summary modes keep their existing visibility semantics.
The built-in `shell` and `gpt_shell` tools are the narrow presentation exception:
the CLI reads their start arguments solely to retain the configured `timeout`
(or the shell provider's 300-second default) and renders their duration chip as
`elapsed/timeout`s. It does not interpret any other shell argument or alter
generic tool-header behavior.
The bundled Swarm `task_blocker` tool, including a structurally prefixed name
such as `work_task_blocker`, is the narrow exception to otherwise generic
tool-header projection: its structured start argument contributes only the
validated `add`, `cancel`, or `list` action label. The CLI retains that safe
label through progress, terminal, replay, and cold-attach reconstruction, while
never projecting the blocker's title, description, answer, reason, or other
payload fields, including in full tool-display mode. An absent or malformed
action fails closed to the identity, lifecycle status, and duration only.

Self-`compact` is the narrow lifecycle exception to generic tool-row
projection. When a durable accepted request proves the caller and target are
the same agent, its visible tool is `compact`, and its request/call correlation
matches the standalone start's request, caller, call, prompt, and transaction,
the CLI repaints that existing generic tool row with the private compaction
lifecycle. The background tool terminal retains ownership of the final generic
result. Missing, late, or contradictory correlation fails open to independent
rows; it never merges a different self request, an `agent_compact` request, or
another standalone compaction. The presentation-only correlation moves with the
owning detached transcript so a reconstructed late tool start can adopt its
known lifecycle state during attach.
Successful standalone lifecycle rows render durable boundary measurements as
`compact #before → #after (retained%) ok`. Estimated chips carry a leading `~`;
one-sided measurements retain the arrow and show `?` for the missing side, while
fully absent measurements degrade to `compact ok`. A zero before count suppresses
the ratio. The literal `ok` remains the final visible token. Provider stream
content remains private, and live, late-attach, and cold-replay rendering all use
the same canonical boundary fields.

Prompt completion may read the local filesystem and query `git` for tracked and
unignored files. These operations should stay bounded and best-effort: failures
or quota/size limits should disable the completion source or surface a local
notice, not wedge the prompt.

Theme completion and no-argument `:theme` listings may inspect custom theme
files only for optional display metadata. These reads must remain best-effort
and bounded: avoid opening non-regular or special theme directory entries, do
not follow symlinks in the metadata path, keep a byte limit for regular files in
case of races, and list malformed, oversized, unreadable, or special entries by
name with an empty description instead of blocking or failing the prompt.

The hidden `tau dev tmux` helper is trusted local testing infrastructure, not a
sandbox. It starts Tau under scratch HOME/XDG paths to avoid accidental config
or state writes during manual E2E checks, but it still runs local processes with
the user's permissions. Scratch cleanup must remain guarded by a helper marker
and path validation so `--remove-scratch` cannot recursively delete arbitrary
user directories. Target commands such as capture, send, and stop must validate
the recognized helper marker and scratch-root shape before connecting to a tmux
socket, and cleanup must validate that ownership before killing a session or
removing the scratch root.

Provider credentials for `tau dev tmux start` are local-only by default. The
helper must not copy providers, tokens, API keys, provider config, or
provider state from the user's real Tau directories unless the user explicitly
opts in through `testing.yaml`. That allowlist names exact extension/provider
pairs only; the helper may copy only the corresponding credential-free settings
file and typed credential subtree into scratch state, must not copy general
config, sessions, logs, unrelated providers, or "all providers", and
must refuse symlink/path-traversal attempts around those files. Reused scratch
destinations must be reconciled to the current allowlist
and must not write through pre-existing symlinks, non-regular files, or
externally linked entries. Missing or empty testing configuration must be
surfaced as a warning and must continue with no provider credentials in the
scratch environment.

The manual boundary and observable helper behavior are recorded in
[`SPEC-tau-cli-dev-tmux`](SPEC-tau-cli-dev-tmux.md).

Raw terminal mode is a process-local ownership boundary. Before spawning editors
or pickers, Tau must pause redraws, release raw-mode features, and always clear
that paused state when ordinary setup or resume fails so the UI cannot remain
permanently muted. Foreground process-group restoration is the narrow exception:
if Tau cannot confirm that it regained foreground ownership after settling the
child, it must not resume raw input or redraw and must exit only the affected
interactive attachment without terminal cleanup writes. Abort paths for
terminal-releasing shell actions should terminate the owned process group before
Tau resumes raw-mode/redraw ownership. Redraw and input coordination assumes a
single foreground reader thread; background renderer threads must not write
while the terminal is released to an external program.

Agent-selection input routing is mirrored immediately on the input thread so a
prompt submitted during renderer handoff reaches the new target. The renderer
must separately publish transcript, selected target, status, and placeholder
changes as one redraw-suppressed transaction, preventing a visible frame from
mixing state derived from different selections.

The CLI owns local terminal commands and parsing, completion, and echo for
harness-owned prompt commands. Dynamic extension actions are resolved against the
current published action schema, while harness-owned prompt commands remain prompt
input for harness resolution. Cross-boundary commands such as `:retry` and `:tree`
parse in the CLI but address exact harness-owned prompt work or provenance rather
than reconstructing it locally. Their behavior is specified by
[SPEC-tau-cli-command-mode](SPEC-tau-cli-command-mode.md).

The CLI also owns presentation-only recursive watch activity. Its current
implementation folds the harness-owned live watch DAG and uses the complete
generic agent-stats runtime snapshot, with active-prompt fallback only before
stats arrive, for `running` and transitive `watching` row presentation plus the
session-wide side-agent count. A separate cycle-safe graph projection selects
the visible deduplicated closure from the viewed agent through eight rows and
falls back to every direct watch on overflow. Current-session semantic
`WorkStatus` snapshots own watched-row lifetime: absent or
`unreported`, `working`, `waiting`, `blocked`, and `unknown` statuses remain visible, and
only `done` hides the row without stopping traversal to its descendants.
Agent-stats runtime state remains the
running-activity authority and never adds or removes a row.
This projection must not create protocol facts,
model-visible notifications, navigation state, persistence, or routing behavior.
Its authority and exact presentation are specified by
[SPEC-tau-cli-agent-message-labels](SPEC-tau-cli-agent-message-labels.md).

Visible transcript state lives in renderer fields; hidden agent and protected
no-agent transcripts live in detached `AgentUiState` presentation models. Hidden
folding mutates only the owning detached model and never swaps or clones the
selected terminal snapshot. Selection materializes the destination model into
the terminal once before publishing editor context or accepting cloned-handle
output. The resulting behavior is specified by
[SPEC-tau-cli-transcript-context](SPEC-tau-cli-transcript-context.md).

The process-local verbose-mode flag is a top-level projection over those
retained presentation models. Verbose mode preserves the configured `show-*`
rendering. Compact mode replaces thinking, terminal tool history/summaries,
turn stats, and diagnostic notices with position-stable empty blocks while
projecting each live tool as one identity/status row without payload. Responses,
alerts, and critical notices remain visible. Terminal tool outcomes remove the live
row through the ordinary lifecycle path. Switching modes re-renders retained
blocks and does not mutate `CliState`, protocol events, journals, or model
context.
See [SPEC-tau-cli-notice-filtering](SPEC-tau-cli-notice-filtering.md).

The visible, hidden, and no-agent presentation models and retroactive-render
caches retain accepted transcript data until an explicit new-session reset or
interactive UI process exit. They have no aggregate item or byte eviction.
`redraw_history_size`, cold-attach staging, and renderer FIFO limits do not
bound retained presentation state. Long-running or high-volume UIs can
therefore consume increasing memory and make selection, resize, and
retroactive display toggles expensive.

The socket reader admits decoded deliveries to one FIFO bounded at 1,024 items
and 64 MiB of encoded frames. Full admission backpressures socket reading and
never drops a decoded delivery. Socket disconnect is the final item in that
same FIFO, so it cannot overtake prior deliveries. Local selection, settings,
action ownership, and timer commands use a separate queue. Each local command
captures the current remote-admission watermark. The renderer drains that
finite prefix, executes the local command, then resumes later remote arrivals;
socket facts cannot be overtaken and continuous remote traffic cannot starve
selection or action ownership. A shared admission arbiter linearizes remote
reservations, local watermark capture, and the scheduler's nonblocking channel
selection. Successful enqueues signal one shared, payload-free, coalesced wake;
the scheduler reruns the same arbiter after each wake and otherwise waits until
the exact renderer sampling deadline. Input routing mirrors selection before
enqueue, and renderer queue pressure never blocks the input thread's direct
harness uplink.

When dequeuing remote work, the scheduler captures the current admission
watermark and may fold only the queued contiguous prefix of pure, matching
provider response updates before the next semantic, UI, disconnect, or local
watermark barrier. It does not wait for a suffix. Every original frame keeps
independent byte/item release and delivery diagnostics even though the folded
run performs one response projection.

During initial cold attach, the UI retains the replay marker through socket
decoding and stages visible replayed prompt/response transcript rows until the
non-replay `session.replay_complete` boundary. Replay-marked current-state rows
continue directly to the renderer, so session, extension, context, and agent
initialization state appears before historical conversation without changing
wire delivery or shared catch-up semantics. Staging uses the same 1,024-item /
64-MiB aggregate limits as renderer admission across retained transcript,
pending tool starts, buffered live tool frames, and session/membership/ownership
indexes. Replayed provider-declared
tool calls authorize only matching durable starts owned by agents currently
loaded in this session; canonical terminals close those starts, including
provider-projected errors. A buffered pre-terminal progress frame retains its
authorized replay start as a temporary renderer owner, so the terminal removes
the live row rather than leaving ownerless multiline output. The UI publishes
that baseline, then starts and progress frames only with an owner through their
first terminal. The first terminal remains visible even without a materialized
start, while later starts or progress frames are suppressed. The fold disables
unconditionally. Encountering tool-bearing history flushes retained plain
history because tool reconstruction has cross-event ordering dependencies.
If live retention reaches the aggregate bound, the UI flushes the reconstructed
baseline and buffered live frames deterministically. Historical overflow or any
scope-index update that cannot fit instead clears the incomplete baseline and
suppresses later replayed starts through the boundary. Both paths disable metadata
observation rather than dropping or growing without bound. Traffic after
the boundary and every non-attach UI passes through directly.

External-editor prompt trailers are prompt-surface text. They may quote
assistant responses and prior prompt text to help compose the next prompt, but
the terminal UI must scope response context to the currently visible/no-agent
transcript and must not let hidden-agent rendering publish a different agent's
response into the shared editor context.

Transcript Markdown-lite formatting is a presentation-only terminal UI feature.
It must not change protocol events, persisted logs, model context, or non-UI
clients, and it must produce only Tau styled text spans rather than raw terminal
escape sequences. Keep its scope narrow to submitted user prompts, assistant
responses, and thinking text; do not accidentally run it over tool output, shell
output, or other machine-generated blocks where styling could obscure exact
results. Markdown table padding is also display-only: it may add spacing around
cell contents for readability, but must preserve the cell text, avoid code
contexts, and keep bounded output amplification. Its width and alignment
projection uses the terminal's grapheme display-column rules and the same
visible-link choice as final span emission, so an OSC 8 label does not reserve
space for its hidden target. Delimiter markers select left, right, or centered
cell placement without changing raw provider text; the projection rejects rows
or aggregate padding beyond its fixed bounds before allocating formatting.

The CLI `redraw_history_size` setting bounds only how many already-rendered
history rows the terminal UI replays to stdout when rebuilding Tau-owned
scrollback after a full redraw. It does not truncate in-memory UI state,
protocol events, durable session logs, provider/model context, or any other
non-terminal history.

`tau --ephemeral` is a session-persistence mode, not a privacy sandbox. It
prevents the current harness process from writing session membership logs,
session metadata/locks, per-session debug `events.jsonl`, per-session
harness/extension stderr logs, session-scoped extension data, and terminal UI
logs. Agent transcripts remain durable under the global agent store unless an
agent is explicitly staged as ephemeral with `:new` then `:ephemeral on`.

Ephemeral agents are also local Tau persistence controls, not confidentiality
boundaries. Their own semantic transcript, metadata, durable session membership,
ephemeral-agent debug JSONL entries, and prompt-history rows stay memory-only
while the daemon lives, but durable recipients/parents may persist projected
messages or results, and provider state, credentials, user/cache extension data,
configuration files, runtime sockets, external services, interceptors, and
trusted tools/extensions keep their normal persistence and filesystem access. Do
not use session or agent ephemerality as a guarantee that prompt contents, tool
results, or extension-observed data cannot be persisted elsewhere.

Future event kinds that carry agent prompts, provider output, tool payloads, or
extension-observed content must update the durable debug-log suppression rules
and regression tests before they are emitted for ephemeral agents.

`tau dev print-prompt` and `print-tools` launch a session-ephemeral harness,
configure ordinary extensions, and load one ephemeral preview agent through
bounded context readiness. Session, preview-agent, journal, transcript, debug,
and retention semantics remain process-local or omitted, so the commands create
no resumable session or agent and do not create or open the durable agent store.
Extensions retain ordinary User, Cache, Secret, direct-state, filesystem,
network, and external-service reads and writes.
Both commands resolve one effective model/tool snapshot and do not call a
provider. `print-system-prompt` retains the separate harness-wide MemoryOnly
policy. Each owned preview runtime socket/discovery pair exists only while the
child runs; the parent removes its exact pair after child reap, including handled
forced-exit fallback.

Protocol-I/O debug counters are diagnostic metadata. They may reveal configured
extension names, message/event names, activity rates, frame counts, and encoded
byte sizes even when the requesting UI did not subscribe to the underlying
events. Per-extension stats therefore require the local socket control path, are
returned only as a directed non-persisted notice, and must remain bounded by
key-cardinality caps with overflow buckets so a noisy peer cannot grow daemon
memory by emitting many unique custom event names.

The local `:debug-show-ui-event-stats` report preserves its lifetime cumulative
totals and additionally reports an attach-phase by delivery-kind matrix. Initial
traffic, including the non-replay `session.replay_complete` boundary, is cold
attach; traffic after that boundary is steady. Replay/non-replay remains an
independent axis, so later agent replay is visible as steady replay. The report
includes exact encoded byte totals plus bounded size distributions for selected
payload components and equality classifications; it never reports payload
contents. Only interactive chat opts into this detailed work; extension meters
remain cumulative-only. Interactive chat reconstructs pending rows by folding
durable historical `tool.started` dispatch facts against canonical lifecycle
terminals through `session.replay_complete`, then applies tool frames that arrived
live during catch-up. Requests, including rejected, unrouted, duplicate, and stale
requests, never create pending presentation. A background placeholder closes the
foreground provider turn but keeps the row pending until the real background
terminal. Generic UI, provider, extension, and restore subscriptions remain
independent of the chat allow-list.

## Navigation projection

The CLI caches harness-owned navigation classification only from complete
`agent.stats_updated` snapshots. Selected transcript, drafts, editor state, and
presentation remain local to each UI.
Submitting a prompt does not optimistically mutate this cache. For accepted
visible prompts to existing targets, the harness applies the implicit `active`
write and the later complete stats snapshot is authoritative across submitting,
observing, reconnecting, and replaying UIs.

The local previous/next cycle includes the no-selection overview alongside the
active agents. That overview remains the start-new-agent input target and owns a
deduplicated presentation of inter-agent messages without changing their durable
sender/recipient projections or prompt routing. It is renderer-local: attachment
catch-up is limited to projections replayed for currently loaded agents, not a
new durable session-wide message index.

`tau session list` uses runtime paths only to locate socket candidates. Each
responsive harness returns its in-memory current session id and immutable
canonical startup project root through a directed local control RPC; persisted
directories and runtime metadata never supply records or fields. Bare output
sorts, deduplicates, and escapes ids into line- and ANSI-control-safe records.
`--dir` canonicalizes an existing caller directory and filters by exact root
identity. `--json` emits one complete array with one two-field record per
responsive harness, including duplicates, so automation can distinguish zero,
one, or multiple matching harnesses. The runtime scan and protocol authority are
governed by
[ARCH-tau-harness](../../tau-harness/specs/ARCH-tau-harness.md) and
[SPEC-tau-proto-session-events](../../tau-proto/specs/SPEC-tau-proto-session-events.md).
Relative filters resolve from caller CWD; missing, inaccessible, and
non-directory values fail as CLI misuse with exit 2. Zero, one, and multiple
matches plus broken output pipes succeed. Bounded discovery, probe,
serialization, and non-broken-pipe output failures return another nonzero
status. Discovery, probe, and serialization failures occur before stdout is
touched; non-broken-pipe stdout failures may leave a prefix because the stream
cannot be rolled back. Listing is inspection-only and never creates or removes
runtime or persisted state.

`tau agent list` obtains membership, runtime, and navigation authority through
the harness's directed current-session roster RPC, then owns filtering, stable
parent-before-child TSV ordering, and escaping. The C-b binding and
`:pick-agent` command invoke the active picker; `:pick-agent-all` invokes the
all-agent picker. Both invoke `fzf` directly through `tau-cli-term`, which
projects width-aware aligned display columns, including each agent's canonical
self/inclusive creator-subtree estimated cost pair when available and a compact
emoji projection of its canonical work-status phase and current-turn state,
plus its canonical title, without changing stable row identity.
Human selected-agent and watched-agent rows prefix identity with adjacent
work-status and detailed-turn emoji, in that order. Stable `tau agent list` TSV keeps machine-facing
text and exposes the detailed activity as a textual field rather than adopting
that visual prefix.
The presentation omits role and the lifecycle field that is constant for
picker-eligible rows. Membership and work status
come from the fresh roster RPC, while cost comes from the input
loop's latest renderer-processed `agent.stats_updated` projection. The pair may
therefore be absent or lag the roster; the picker neither creates an atomic
cross-source snapshot nor locally reprices usage.
Active filtering uses navigation mode plus runtime eligibility
without replacing the independent current-turn display; the all-agent
action includes every current live navigation mode. The underlying actions
remain configurable, and the all-agent action has no built-in key binding. The
CLI revalidates the chosen agent against the same category with a second
snapshot and uses the existing local selection transition. Picker cancellation
and failure do not retarget the prompt draft. This eligibility projection follows
[SPEC-tau-proto-session-events](../../tau-proto/specs/SPEC-tau-proto-session-events.md).
The provider setup CLI owns credential-free per-instance settings publication
and harness-layout secret initialization. Exact target selection and
secret-first/settings-last ordering follow
[SPEC-extension-secret-storage](../../../specs/SPEC-extension-secret-storage.md).
