# SPEC-tau-cli-transcript-context: Transcript and editor context

## Record justification

Transcript ownership crosses event rendering, per-agent UI snapshots, overview
state, prompt-editor context, durable replay, and harness-owned navigation
requests. These cooperating areas collectively define which visible state
follows an agent or editor and cannot be documented coherently beside any one
renderer or input-loop implementation.

The terminal UI keeps visible transcript state in renderer fields and detached
hidden-agent presentation models in `AgentUiState`. Response text used by the external
prompt editor's trailer follows the same per-agent snapshot boundary: current and
last assistant response context belongs to the viewed/no-agent transcript, while
prompt-local fields such as previous prompt and trailer recovery stay with the
active input/editor flow.

Live UI blocks that have a distinct start/completion lifecycle must complete in
the same transcript presentation model that rendered their start block, even if
the user switches viewed agents before completion arrives. Hidden completion
folding updates or removes the live block directly in the owning agent or
no-agent model without replacing the actually visible transcript or publishing
hidden prompt-editor context.

While an event folds into a hidden transcript, shared input-loop mirrors and
cloned terminal handles must continue to expose and append to the actually
visible transcript. The hidden fold becomes visible only when that transcript is
selected.

The initial no-agent/start-new-agent screen is not a durable transcript boundary.
Startup or post-`:session new` status, action, and extension output that is
visible there is the beginning of the first selected/created agent conversation.
Selecting that first agent therefore adopts the visible no-agent output in place,
without replacing the terminal snapshot or clearing scrollback. Pending no-agent
action completions and extension lifecycle owners are retargeted to the adopted
agent only in this initial no-swap case so later completions update the same
visible conversation.

The exception is all-agent overview history. The no-agent screen copies each
genuine inter-agent message into a session-scoped aggregate, deduplicating sender
and recipient projections by their originating session and shared message id,
and continuing to apply the configured `show-messages` mode. `Message`,
`WatchResponse`, and `WatchPrompt` are the only overview content kinds;
`WatchProviderStatus`, `WatchWorkStatus`, and `WatchLongWait` records stay in
the watcher's transcript. Once the aggregate has an entry, its
snapshot is protected even on the initial screen, and selecting or creating an
agent restores that agent's own transcript instead of adopting overview history.
The original sender and recipient projections remain in their respective agent
transcripts.

Internal prompt facts use their typed `submission_source` at the same live or
replay position. `Extension { name }` renders once as an attributed message;
typed `HarnessInternal` renders its plain payload in the dedicated
`system.internal_notice` style only when the default-off
`show_internal_prompts` setting is enabled; `Legacy` stays hidden. The built-in
themes italicize this style. Typed watch-provider notifications use the same
compact notice presentation without a textual provenance label or the
provider-only outer envelope. Live and initial-snapshot notifications use the
same representation because typed provenance and event state retain that
distinction. The renderer removes only an exact canonical
`<tau_internal>...</tau_internal>` outer frame; partial, nested, nonmatching,
and legacy text remains verbatim. Replay and live rendering use the same
projection. See
[SPEC-tau-cli-agent-message-labels](SPEC-tau-cli-agent-message-labels.md) for
the corresponding typed message classification.
The `internal_kind=context_size_alert` presentation takes precedence and always
renders its exact text once. Missing tags, `ctx_id`, and prompt text never
imply trusted provenance or special presentation. This behavior is confirmed by
[SPEC-compaction-and-context-recovery](../../../specs/SPEC-compaction-and-context-recovery.md).
The `internal_kind=background_tool_completion` lifecycle notice never
renders in the human transcript, even when `show_internal_prompts` is enabled.
This suppression does not apply to untyped or differently typed internal text,
including text with the same spelling.

Visible prompt facts, prompt recall/history, and transcript snapshots render the
raw accepted canonical text. The provider-only `<user>` projection derived from
typed `HumanUi` provenance never enters CLI display, editor state, or navigation
anchors. See
[SPEC-interactive-user-prompt-envelope](../../../specs/SPEC-interactive-user-prompt-envelope.md).

The overview is renderer-local rather than a durable session index. It contains
message projections observed by that CLI plus catch-up projections the harness
replays for agents that are currently loaded when the CLI attaches. If both
endpoints unloaded before attachment, their earlier messages are absent from that
new CLI's overview; no extra persistence or session-wide replay authority exists
solely for this presentation.

Explicit `:agent none` and `:agent new` after leaving an agent also create a
protected no-agent snapshot, and fresh agents must not inherit output or pending
owners from that explicit global view. The no-agent screen remains the
start-new-agent input target as well as the all-agent overview.
