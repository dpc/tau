# ARCH-tau-cli: tau-cli architecture

The CLI consumes harness-validated provider-neutral quota snapshots and applies
the fixed weekly pacing classifier from
[SPEC-provider-quota-pacing](../../../specs/SPEC-provider-quota-pacing.md).
It selects only an exact current/viewed `ModelId` binding, preserves provider
timestamps during catch-up, keeps per-cycle hysteresis locally, and renders the
accessible compact `Q-`, `Q=`, `Q+`, `Q!`, or `Q?` status chip.
The lightweight `agent.prompt_started` event supplies the selected agent's
model. Provider quota current-state is capability evidence for neutral `Q?`;
only a fresh exact binding and trustworthy weekly timing permit colored pacing.
Capability lasts for the running harness: a replayed empty snapshot after
provider clear keeps live and late clients converged on neutral unknown.

The terminal UI executes trusted local configuration and environment-derived
commands, including key-binding shell snippets, completion commands, `$EDITOR`,
and `$VISUAL`. Treat `cli.yaml`, inherited environment variables, and PATH as
local code execution inputs rather than untrusted data.

Prompt completion may read the local filesystem and query `git` for tracked and
unignored files. These operations should stay bounded and best-effort: failures
or quota/size limits should disable the completion source or surface a local
notice, not wedge the prompt.

Theme completion and no-argument `/theme` listings may inspect custom theme
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
helper must not copy provider profiles, tokens, API keys, provider config, or
provider state from the user's real Tau directories unless the user explicitly
opts in through `testing.yaml`. That allowlist names exact provider profile
names only; the helper may copy only the corresponding
`auth.d/<provider>.json` files into scratch state, must not copy lock files,
general config, sessions, logs, unrelated provider profiles, whole directories,
or "all providers", and must refuse symlink/path-traversal attempts around those
files. Reused scratch destinations must be reconciled to the current allowlist
and must not write through pre-existing symlinks, non-regular files, or
externally linked entries. Missing or empty testing configuration must be
surfaced as a warning and must continue with no provider credentials in the
scratch environment.

The manual boundary and observable helper behavior are recorded in
[`SPEC-tau-cli-dev-tmux`](SPEC-tau-cli-dev-tmux.md).

Raw terminal mode is a process-local ownership boundary. Before spawning editors
or pickers, Tau must pause redraws, release raw-mode features, and always clear
that paused state when setup or resume fails so the UI cannot remain permanently
muted. Abort paths for terminal-releasing shell actions should terminate the
owned process group before Tau resumes raw-mode/redraw ownership. Redraw and
input coordination assumes a single foreground reader thread; background
renderer threads must not write while the terminal is released to an external
program.

Agent-selection input routing is mirrored immediately on the input thread so a
prompt submitted during renderer handoff reaches the new target. The renderer
must separately publish transcript, selected target, status, and placeholder
changes as one redraw-suppressed transaction, preventing a visible frame from
mixing state derived from different selections.

The CLI owns local terminal commands and parsing, completion, and echo for
harness-owned prompt commands. Dynamic extension actions are resolved against the
current published action schema, while harness-owned prompt commands remain prompt
input for harness resolution. Cross-boundary commands such as `/retry` and `/tree`
parse in the CLI but address exact harness-owned prompt work or provenance rather
than reconstructing it locally. Their behavior is specified by
[SPEC-tau-cli-slash-commands](SPEC-tau-cli-slash-commands.md).

The CLI also owns presentation-only recursive watch activity. It folds the
harness-owned live watch DAG and edge-scoped outer-turn lifecycle into direct
`running` and transitive `watching` rows plus the session-wide side-agent count.
This projection must not create protocol facts, model-visible notifications,
navigation state, persistence, or routing behavior. Its authority and exact
presentation are specified by
[SPEC-tau-cli-agent-message-labels](SPEC-tau-cli-agent-message-labels.md).

Visible transcript state lives in renderer fields; hidden agent and protected
no-agent transcripts live in `AgentUiState` snapshots. Hidden folding temporarily
restores the owning snapshot under the terminal-output lock, then restores the
visible snapshot before publishing editor context or accepting cloned-handle
output. The resulting behavior is specified by
[SPEC-tau-cli-transcript-context](SPEC-tau-cli-transcript-context.md).

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
contexts, and keep bounded output amplification.

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
agent is explicitly staged as ephemeral with `/new` then `/ephemeral on`.

Ephemeral agents are also local Tau persistence controls, not confidentiality
boundaries. Their own semantic transcript, metadata, durable session membership,
ephemeral-agent debug JSONL entries, and prompt-history rows stay memory-only
while the daemon lives, but durable recipients/parents may persist projected
messages or results, and provider state, credentials, user/cache extension data,
policy/config files, runtime sockets, external services, interceptors, and
trusted tools/extensions keep their normal persistence and filesystem access. Do
not use session or agent ephemerality as a guarantee that prompt contents, tool
results, or extension-observed data cannot be persisted elsewhere.

Future event kinds that carry agent prompts, provider output, tool payloads, or
extension-observed content must update the durable debug-log suppression rules
and regression tests before they are emitted for ephemeral agents.

Protocol-I/O debug counters are diagnostic metadata. They may reveal configured
extension names, message/event names, activity rates, frame counts, and encoded
byte sizes even when the requesting UI did not subscribe to the underlying
events. Per-extension stats therefore require the local socket control path, are
returned only as a directed non-persisted notice, and must remain bounded by
key-cardinality caps with overflow buckets so a noisy peer cannot grow daemon
memory by emitting many unique custom event names.

## Navigation projection

The CLI caches harness-owned navigation classification only from complete
`agent.stats_updated` snapshots. Selected transcript, drafts, editor state, and
presentation remain local to each UI.

The local previous/next cycle includes the no-selection overview alongside the
active agents. That overview remains the start-new-agent input target and owns a
deduplicated presentation of inter-agent messages without changing their durable
sender/recipient projections or prompt routing. It is renderer-local: attachment
catch-up is limited to projections replayed for currently loaded agents, not a
new durable session-wide message index.

`tau agent list` obtains membership, runtime, and navigation authority through
the harness's directed current-session roster RPC, then owns filtering, stable
parent-before-child TSV ordering, and escaping. The C-b action invokes `fzf`
directly through `tau-cli-term`, which projects width-aware aligned display
columns without changing the stable TSV selection row. The CLI revalidates the
chosen current non-suspended agent with a second snapshot and uses the existing
local selection transition. Picker cancellation and failure do not retarget the
prompt draft.
