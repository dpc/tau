# tau-cli design decisions

This file records local terminal-UI design decisions that future changes should
preserve unless the project intentionally revisits them. It complements the
crate README/AGENTS instructions with durable rationale for transcript rendering
and other UI boundaries.

## Markdown-lite transcript styling

Status: confirmed, 2026-06-15, dpc


Tau applies Markdown-lite formatting in the terminal UI only. The harness,
protocol events, durable agent logs, prompt previews, model context, and other
clients continue to see the original plain text.

The formatter is deliberately small. It recognizes headings, unordered and
ordered list markers, `*strong*` / `**strong**`, `_emphasis_`, combined
`***strong emphasis***`, `~~strikethrough~~`, basic backslash escapes, and
leading-pipe tables. Triple-asterisk runs compose strong and emphasis styles,
while strikethrough uses its own semantic style; this does not introduce a
general CommonMark parser. Most
constructs are style-only and preserve exact source characters rather than
stripping delimiters or rewriting list/header prefixes. Tables are the exception:
the UI may add bounded display-only padding spaces so cells align while the
visible text remains valid Markdown table syntax. Inline backticks, fenced code
blocks, and indented code-like lines get code styling and suppress nested
Markdown-lite styling; escaped marker sequences get escape styling. This keeps
live terminal wrapping, scrollback, and copy/paste behavior stable outside
intentional table padding.

Live response and thinking blocks use an append-aware cache. Text before a blank
line is treated as sealed and parsed once; the current unsealed suffix remains
base-styled until a future update seals it. The cache also preserves parser
context, including open fenced code blocks, across sealed chunks. Final/static
blocks parse the complete string immediately.

Formatting is scoped to submitted user prompts, assistant response text, and
reasoning/thinking text. Tool calls, tool payloads/results, shell output,
status/progress lines, and agent-to-agent message debug displays must stay on
their existing renderers unless there is a separate product decision.

Agent turn stats are a standalone live-indicator status line. The CLI may
remember the latest `agent.turn_stats_updated` sample for an in-flight prompt
only to repaint the transient ellipsis block, and derives bytes/rate from the
event's `current` and `previous` samples. The CLI renders a generic
`(bytes, bytes/s)` suffix only on that transient indicator, not on visible
assistant text, and must not copy stats text into editor current-response state,
prompt-stdin capture, durable transcripts, or final response rendering.

## New-agent staging

Status: unconfirmed

`/new` enters a local "next prompt creates an agent" mode. `/new <role>` also
requests that role and stores it as a one-shot latency bridge so an immediate
prompt can create the new agent with the requested role before the asynchronous
`harness.role_selected` echo updates the UI role mirror. Later no-agent role
selection commands, including `/role <role>` and role cycling, supersede that
bridged role; the staged role is not a hidden durable role authority.

Options such as `/model <provider>/<model>` and `/ephemeral [on|off]` stage
one-shot properties for that next `ui.create_agent`; they are consumed by the
first prompt that creates the agent and cleared when the UI switches to an
existing agent or a new session. Bare `/new` clears only a stale bridged role,
while preserving staged model and ephemeral options. Bare `/ephemeral` toggles
the staged memory-only flag, while `/ephemeral on` and `/ephemeral off` set it
explicitly. These commands do not convert existing agents in place.

## Bundled component launcher

Status: confirmed, 2026-06-17, dpc

The unified `tau` binary launches in-process bundled programs with the
`tau component <component>` subcommand. This vocabulary is intentionally broader
than "extension": bundled extensions such as `ext-shell` and
`ext-provider-builtin` are components, but the harness is also a component and
is not an extension. Internal harness startup and built-in extension defaults
should therefore use `tau component harness` and `tau component <extension>`;
`tau ext <name>` is not a supported compatibility alias.

## Notice filtering

Status: confirmed, 2026-06-17, dpc

Harness/UI notices are filtered in the terminal UI, not at the harness emission site. The default threshold is `info`; `/set notice-level <level>` and persisted `cli.json` `notice_level` change what routine notices a UI renders. Critical notices and `always_show` warning diagnostics remain visible regardless of threshold. UI special-casing must use the stable `harness.notice.kind` field rather than parsing notice text.

## Slash command ownership

Status: unconfirmed

The terminal input loop has multiple slash-command owners. CLI-owned commands
such as `/quit`, `/session`, `/agent`, `/name`, `/role`, `/model`, `/set`, and
`/theme` are handled locally. Dynamic extension actions are parsed against the
current published action schema and dispatched as `ActionInvoke` events.
Harness-owned prompt commands, currently `/skill <name> ...` and
`/skill:<name> ...`, are completed and echoed by the CLI but must still be
submitted as prompts so the harness can resolve skills and inject their content.

Until action schemas can mark sensitive arguments, the CLI has one narrow
action-specific redaction exception: `/email auth google finish ...` is redacted
in command echo and persistent prompt history because its pasted loopback URL
contains a one-time OAuth authorization code. The raw `ActionInvoke` still goes
to the owning extension so the action can complete; future schema/protocol
sensitive-argument metadata should replace this hard-coded action id.

`/model <provider>/<model>` has two CLI-owned paths: with a selected agent it
emits a targeted `ui.agent_model_select`; after `/new`, with no selected agent,
it stages a one-shot `ui.create_agent.model_override` for the next prompt-created
agent instead of sending an untargeted agent update.

Agent switch commands distinguish known transcript selection from active prompt
routing. `/agent switch` completions list active agents and `none`, keeping
suspended agents out of ordinary switch suggestions. An explicitly typed known
suspended agent id is still accepted so the UI can view that transcript; prompt
submission remains blocked while the selected agent is suspended until `/agent
resume` or `/resume` marks it active again.

`/name <display name>` is the selected-agent shortcut for `/agent name
<agent_id> <display name>`. It emits the same display-name update as `/agent
name` after resolving the currently selected agent, matching current-agent
shortcuts such as `/suspend` and `/resume`.

Only after those owners decline a line may the CLI treat an unrecognized leading
slash token as an unknown-action notice. That fallback is intentionally limited
to leading slash roots; ordinary prompt text that contains slashes later in the
line remains normal prompt text.

`/tree` argument parsing is CLI-owned, while anchor resolution is harness-owned.
The CLI maps `/tree <positive-integer>` to a one-based prompt anchor target,
`/tree 0` and `/tree root` to the explicit root/before-first target, and
`/tree node <non-negative-integer>` to the raw-node expert target. It must not
send bare numeric arguments as raw transcript node ids; the harness resolves
prompt anchors against the selected agent's durable prompt provenance.

## Theme defaults

Status: confirmed, 2026-06-17, dpc

The built-in `tau-plain-dark` theme is intentionally conservative. It keeps
semantic text attributes such as bold, italic, underline, and strikethrough, and
limits hard-coded foreground colors to default color plus yellow, cyan, green,
and red. Those colors are considered generally safe terminal colors, while other
`tau-dpc` theme colors are dropped or mapped so Tau remains readable on unusual
terminal palettes. More opinionated built-ins, including the personalized
`tau-dpc` theme and the light-background `tau-plain-light` theme, remain
selectable but are not the default.

## Manual tmux E2E helper

Status: confirmed, 2026-06-18, dpc

Manual terminal end-to-end checks should use the hidden `tau dev tmux` helper.
That helper is the accepted tmux-only boundary for agent-controlled manual Tau UI
testing: it starts a real Tau binary in a private tmux server, defaults to
scratch HOME/XDG state, and keeps the workflow manual rather than turning tmux
into a second automated test framework. The outer `tau dev tmux` dispatch path
must not load or validate the caller's normal harness configuration before
spawning the scratch child Tau; startup overrides that would require normal
harness config resolution are rejected at the outer helper boundary.

`tau dev tmux start` owns scratch-root generation: when no root is supplied, it
chooses a fresh temporary root and prints it before fallible scratch/provider
setup so failed starts remain easy to clean up. Target commands (`capture`,
`send`, and `stop`) keep the deterministic historical fallback root when no root
is supplied, but normal generated-root workflows should use the printed commands
from `start`.

Provider access in tmux E2E runs is an explicit testing-only exception to the
scratch-state default. `tau dev tmux start` may read only `testing.yaml` from the
real Tau config directory. Missing or empty testing config keeps the child
local-only and must warn. Non-empty `testing_providers` names are exact provider
profile allowlist entries; there is no "all providers" mode. The helper may copy
only corresponding real `auth.d/<provider>.json` files into scratch state, must
not copy provider lock files, general config, sessions, logs, or unrelated
profiles, and must fail closed on path traversal, symlink, non-regular file, or
unsafe destination conditions. `provider-builtin` is enabled in the child only
when the current allowlist is non-empty.

## tau-cli testing strategy

Status: unconfirmed

`dev_tmux` provider-access tests should stay focused on the security boundary:
config parsing, exact allowlist copying, stale scratch reconciliation, warning
behavior, and refusal of symlink, non-regular, path-traversal, or unsafe
source/destination entries.

Pure transcript renderers should be tested at the rendered block/span boundary,
not by snapshotting built-in theme implementation details. Rendering and theme
behavior tests must use representative fixture themes with distinct semantic
attributes, assert exact text preservation except for documented display-only
transforms such as table padding, and check that the resolved spans carry the
intended semantic styling. Built-in theme tests should only validate that the
embedded files parse and satisfy intentional theme-level invariants, so built-in
theme tweaks do not force unrelated renderer expectation churn.

Input-loop command routing tests should cover the emitted local notices and
harness events/prompts produced by routing decisions, not only tokenizer helper
functions. This is especially important for slash-command ownership boundaries
where CLI-owned commands, dynamic extension actions, harness-owned prompt
commands, and the unknown leading-slash fallback intentionally share similar
surface syntax.

Persistent prompt-history storage tests should cover the length-prefixed record
boundary: ordered round trips, bounded/unsupported/malformed records, torn or
oversized tails before append, and redaction/routing at the chat-command layer.
Keep these as focused unit tests around `prompt_history` plus routing tests for
command-line redaction; do not require interactive terminal E2E checks for
storage-format regressions.

## Provider response delta accumulation

Status: confirmed, 2026-06-19, dpc

Terminal streaming accumulates `provider.response_updated.deltas` per prompt and provider output index. If a UI sees a delta for an unknown in-flight prompt, it may create a live block with an ellipsis prefix to indicate missed earlier transient deltas; the final `provider.response_finished` replaces live content with complete durable output. Provider status updates are rendered as transient status text and do not enter assistant response accumulation.

Live prompt output inside the terminal active area has a stable semantic order:
thinking, provider compaction status, assistant response, then active tool
summary/tool-call blocks. New live response-side blocks must be inserted before
active tool anchors rather than appended after them, so running tool UI remains
pinned nearest the prompt while assistant text continues streaming. This ordering
is a CLI renderer responsibility; it should not be implemented by rebuilding the
entire terminal output snapshot or by forcing full redraws of scrollback.

## Per-agent prompt-editor response context

Status: confirmed, 2026-06-25, dpc

The terminal UI keeps visible transcript state in renderer fields and snapshots
hidden agent transcripts in `AgentUiState`. Response text used by the external
prompt editor's trailer follows the same per-agent snapshot boundary: current and
last assistant response context belongs to the viewed/no-agent transcript, while
prompt-local fields such as previous prompt and trailer recovery stay with the
active input/editor flow.

Live UI blocks that have a distinct start/completion lifecycle must complete in
the same transcript snapshot that rendered their start block, even if the user
switches viewed agents before completion arrives. Hidden completion folding may
temporarily restore the owning agent or no-agent snapshot, update/remove the live
block there, then restore the actually visible transcript without publishing
hidden prompt-editor context.

When routing an event for a hidden agent, the renderer may temporarily restore
that hidden snapshot into renderer fields to reuse normal folding code. During
that hidden fold it must not publish hidden response context through shared
input-loop mirrors such as `EditorContext`; Ctrl+O and other prompt actions must
continue seeing the actually visible/no-agent context until the user explicitly
switches transcripts.

The hidden restore/fold/save/restore sequence must also be atomic with respect
to terminal output emitted through cloned `TermHandle`s, such as local
client-side notices. Hidden folding installs a temporary output snapshot in the
shared terminal handle; local output must wait until the actually visible
snapshot is restored so it cannot be appended to a hidden agent transcript by a
cross-thread race.

The initial no-agent/start-new-agent screen is not a durable transcript boundary.
Startup or post-`/session new` status, action, and extension output that is
visible there is the beginning of the first selected/created agent conversation.
Selecting that first agent therefore adopts the visible no-agent output in place,
without replacing the terminal snapshot or clearing scrollback. Pending no-agent
action completions and extension lifecycle owners are retargeted to the adopted
agent only in this initial no-swap case so later completions update the same
visible conversation. Explicit `/agent none` and `/agent new` after leaving an
agent are different: they intentionally create a protected no-agent snapshot, and
fresh agents must not inherit output or pending owners from that explicit global
view.

## Dynamic action completion snapshot ownership

Status: unconfirmed

Dynamic extension action completions (`action.result` and `action.error`) render
in the transcript snapshot that was viewed when the CLI sent the matching
`action.invoke`, not whichever agent is selected when the completion arrives.
The CLI records `ActionInvocationId -> viewed agent/no-agent snapshot` before
sending the invoke because completion events carry an invocation id but no agent
id. Unknown or replayed completions keep the existing visible fallback.
