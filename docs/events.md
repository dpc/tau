# Event log reference

## Canonical transport messages

`agent.message_incoming` and `agent.message_outgoing` are harness-owned,
immutable durable v2 facts carrying a transport-neutral `MessageEnvelope`.
Extensions register a source-bound capability and use dedicated correlated
ingress/completion RPCs; they cannot emit either fact. Ingress success is sent
only after commit. Identical stable-key retries return the original canonical
id, while conflicting reuse fails closed.

An incoming fact folds directly into typed provider context and creates only a
payload-free live wake marker. Replay restores and renders context but never
wakes an agent or activates a reply route. Native identifiers are bounded
durable metadata. A canonical `reply_to` id is an opaque selector rather than a
bearer capability and is reauthorized against the live source, session, agent,
route, and tool at send completion.

The tau bus mostly carries facts: components broadcast what happened, while the
`ui.*` category carries user-intent requests from attached UIs to the harness.
Every event has a dotted name `<category>.<call>` and a typed payload defined in
`crates/tau-proto/src/events.rs`. This selected guide groups the core events by
component (or class of component) that emits them; `events.rs` is the exhaustive
source of truth for every current first-party wire event.

Events are distinct from **messages**: messages are point-to-point protocol
traffic (handshake, subscribe/intercept, `emit`, `deliver`, etc.) and never
appear on the bus or in durable semantic logs. Events are not top-level wire
items; peers send them inside `emit` and receive them inside `deliver`. See
[messages.md](messages.md) for the message-side reference.
Tool registration payloads now include neutral `tags`, and provider model metadata includes model capability `tags`; these are protocol data used by the harness to decide the effective prompt tool surface.

A few categories don't map to a single emitter — those are grouped by the
class of function that raises them.

## Harness (general)

Emitted by the harness daemon itself, mostly for UI-facing status and
for control of the emit/intercept pipeline.

- **`harness.notice`** — A free-form notice from the harness for the user.
  Notices include `kind` (stable machine-readable type), `message`, `level`
  (`critical`, `warning`, `info`, `debug`, or `trace`), and optional
  `always_show`. UIs filter notices locally by their configured notice-level;
  critical and `always_show` notices remain visible. Current first-party kinds
  include `extension.config_error`, `extension.optional_skipped`,
  `extension.notice` for sanitized extension-authored notices,
  `harness.config_error`, `harness.failure`, `harness.internal_warning`,
  `harness.notice`, `harness.replay_error`, `model.selection`,
  `skill.collision`, and `ui.command_error`. Expected skill-name collisions are
  trace-level notices.
  Extension-authored skill diagnostics are sanitized to `extension.notice`; add a
  first-party kind here only when the harness owns and preserves it.
- **`harness.session_dir`** — Announces the current session directory for UIs
  and extensions that need to present or inspect session-local paths. In
  `--ephemeral` mode this carries status `ephemeral` and a display-only
  `<ephemeral>` path because no session directory is written.
- **`harness.ui_dir`** — Announces the UI state directory for UI-facing helpers.
- **`harness.models_available`** — The full provider-published model list
  as `provider/model_id` strings. Re-emitted when provider snapshots change.
- **`harness.roles_available`** — Snapshot of roles currently available from
  effective configuration, including their display descriptions for UIs.
- **`harness.role_selected`** — Which role is currently selected, plus
  the model it resolves to and that model's context-window size if known.
- **`harness.context_usage_changed`** — Updated input/cached token counts
  and percent-of-context-window for the selected role's resolved model,
  after each agent response that reports usage.
- **`harness.agent_context_usage_changed`** — Updated context-usage snapshot for
  a specific agent, used by UIs that render per-agent context pressure.
- **`harness.efforts_available`** — Which effort levels are valid for the
  selected role's resolved model. Empty when the selected role has no
  resolved model or the provider doesn't support reasoning.
- **`harness.verbosities_available`** — Which output verbosity levels are valid
  for the selected role's resolved model. Empty means no resolved model;
  `[medium]` means the provider does not expose a verbosity knob.
- **`harness.thinking_summaries_available`** — Which thinking-summary modes are
  valid for the selected role's resolved model. Empty means no resolved model;
  `[off]` means the provider does not support thinking summaries.

## Session (harness session tracker)

Emitted by the harness's session tracker. The durable session log is a
membership journal, not a transcript.

- **`session.started`** — Must-pass immutable runtime lifecycle fact: the
  harness created or switched to a session. Carries `session_id` and a reason
  (`initial` startup, `new` via `/session new`, `resume` of an existing session).
  Registered session context providers react with per-session setup and reply
  with `extension.session_context_ready`; per-agent context providers react to
  `session.agent_loaded` and reply with `extension.context_ready`. Interceptors
  cannot drop or rewrite it.
- **`session.shutdown`** — Must-pass immutable runtime lifecycle fact: the
  harness is leaving the current session, emitted before `session.started` for
  the next one. Extensions flush or drop per-session state. Interceptors cannot
  drop or rewrite it.
- **`session.agent_loaded`** — Membership fact: a global agent is loaded into
  this session. Durable agents write this to the session log so resume can fold
  the loaded-agent set. Ephemeral agents set `ephemeral: true`; the fact is
  delivered live/replayed while the daemon runs but is memory-only and omitted
  from cold resume. Interceptors cannot drop or rewrite this immutable
  membership fact.
- **`session.agent_unloaded`** — Membership fact: a global agent is no longer
  loaded into this session. Like load facts, this is durable for durable agents
  and memory-only for ephemeral agents. Interceptors cannot drop or rewrite this
  immutable membership fact.
- **`agent.replay_complete`** / **`session.replay_complete`** — Transient,
  harness-owned catch-up boundaries. They are delivered as non-replay frames
  after matching historical facts and before buffered live frames for that
  connection.

Historical load/unload facts are not transcript history. On reconnect/resume the
harness announces the current loaded-agent snapshot, then replays each loaded
agent log once.

When an existing stored agent is loaded into an already-live session,
subscribers may see the live `session.agent_loaded` membership fact before the
per-agent catch-up stream. Restore-aware consumers must treat the matching
replay-marked metadata/history and following non-replay `agent.replay_complete`
boundary as the point at which restored agent state is complete; they must not
publish default/stale agent context merely because the live membership fact
arrived first.
If an `agent.replay_complete` boundary carries `error`, restore-aware consumers
must fail closed for that agent: do not synthesize default restored state or mark
agent context ready from an incomplete replay.

## Agent transcript and prompt lifecycle

Emitted mostly by the harness as it routes UI requests into concrete global
agents. Durable transcript facts are written to the owning agent log, not the
session log. Ephemeral agents use the same event stream while the daemon lives
but keep it in memory only; `agent.started.ephemeral` marks that boundary.

- **`agent.prompt_submitted`** — A `ui.prompt_submitted` request was accepted
  into a concrete agent transcript. Carries `agent_id`, text, originator, and
  user/internal message class.
- **`agent.prompt_queued`** — A prompt arrived while the agent was busy and was
  queued instead of dispatched. Runtime UI state; not durable transcript truth.
- **`agent.prompt_recalled`** — A queued prompt was recalled for editing.
- **`agent.prompt_steered`** — A previously queued prompt is folded into an
  in-flight turn as a steering user message rather than starting a fresh turn.
- **`agent.user_message_injected`** — A synthetic user message inserted by the
  harness (e.g. `!`-shell command output, AGENTS.md preamble). Folds into the
  agent tree like a real user prompt.
- **`agent.prompt_created`** — The harness assembled a provider prompt and
  assigned it an `agent_prompt_id`; payload carries `agent_id`, `session_id`,
  `system_prompt`, materialized `context`, tools or `tools_ref`, model, model
  params, tool choice, originator/provenance, legacy cache-sharing flag,
  optional UI correlation id, and optional compaction summary. First-party
  ChatGPT/Codex cache routing is stable per target agent and does not split on
  those provenance fields. This is
  operational delivery state for the provider; transcript truth is still the
  accepted prompt, provider response, terminal tool results, and compaction
  facts.
- **`agent.prompt_started`** — Lightweight runtime lifecycle companion to
  `agent.prompt_created`. Carries the prompt id, agent id, session id, model,
  originator, and optional UI correlation id, but omits provider prompt content.
  It is emitted immediately before the matching `agent.prompt_created`, is
  transient, and is not replayed from durable agent logs. UIs and observers
  should use this when they only need to track in-flight prompt state.
- **`agent.state`** — Transient live runtime snapshot for one agent. Carries
  `agent_id` plus `idle`/`running` state so UIs can show work in progress
  without treating it as transcript history.
- **`agent.watches_updated`** — Transient session-local full snapshot of the
  agents watched by one watcher agent. Empty watched sets are valid after a
  disable; late subscribers receive only current non-empty snapshots.
- **`agent.stats_updated`** — Transient, content-free operational snapshot for
  one loaded agent: runtime state, current/cumulative tool counters, and latest
  context usage. It replaces the old delegation-specific progress stream.
- **`agent.prompt_terminated`** — A prompt ended without an accepted
  `provider.response_finished` (stale or canceled). Runtime lifecycle state.
- **`agent.prompt_prewarm_requested`** — Best-effort provider cache prewarm for
  the next prompt prefix. Runtime/provider optimization state.
- **`agent.compaction_triggered`** — Durable manual compaction trigger inserted
  into an agent transcript. Prompt assembly folds it into provider-side
  compaction input; it is not a separate compaction lifecycle event.
- **`agent.display_name_set`** — Durable fact that changes an agent's
  human-friendly display name. Carries `agent_id` and the new non-empty display
  name; UIs use it when rendering agent chips and history.
- **`agent.metadata_set`** / **`agent.metadata_unset`** — Durable,
  interceptable per-agent metadata updates. Keys are strings, values are
  arbitrary CBOR capped at 64 KiB, and `metadata_set` carries an `inheritable`
  flag copied to child agents at creation time. Extensions use these facts for
  extension-visible state such as `ext_core-shell_cwd`.
- **`agent.started`** — Creation fact for an agent. Durable agents write it to
  their event log; ephemeral agents replay it from memory only. It carries
  optional `parent_agent`; inheritable metadata from that parent is copied into
  the new agent after this fact commits and before the agent is announced loaded.
- **`agent.head_moved`** — Durable fact that changes an agent's selected tree
  head after navigation, so future prompts branch from the requested root or
  node target.


## Provider execution

Emitted by the provider backend that owns the selected model.

- **`provider.models_updated`** — Provider extension replacement snapshot of
  currently servable models and their capabilities. The harness folds provider
  snapshots into `harness.models_available` and related role/model availability
  events.
- **`provider.prompt_submitted`** — The provider accepted an `agent.prompt_created`
  and started processing it. Echoes the originator. Transient.
- **`provider.response_updated`** — Transient provider-owned live response update.
  `deltas` carry newly appended displayable assistant/reasoning text. `status`
  carries provider-authored retry/diagnostic status. `compaction` carries
  provider-side compaction lifecycle. `response_stats` carries public
  content-free previous/current response-throughput samples for the current
  provider prompt. Providers count backend response bytes at the transport
  receive boundary before semantic parsing and emit stats at the provider's
  rate-limited cadence. The harness validates prompt ownership and routing, then
  broadcasts these updates unchanged; UI clients render stats directly from this
  event. Stats-only updates are valid and transient.

- **`provider.response_finished`** — Final assistant output in original
  item order via `output_items`, plus optional usage, provider
  response id, backend metadata, and echoed originator. Routed by the
  harness based on the originator.
- **`provider.tool_result`** / **`provider.tool_error`** — Provider-facing
  terminal tool-call completions. These satisfy provider protocol state and
  fold into prompt history, but are not logical UI tool completions. The
  synthetic background placeholder uses `provider.tool_result` only.
- **`provider.cache_miss_diagnostic`** — Provider-owned diagnostic for a prompt
  with unexpectedly low cache reuse. The harness accepts it only from the
  provider that owns the prompt, and providers emit it before the matching
  `provider.response_finished` closes the pending provider route.
## Tools

Tool events span three emitters: extensions register/implement tools,
the agent requests calls, and the harness orchestrates dispatch.

- **`tool.register`** *(extension)* — A tool provider advertises a tool
  spec (name, description, JSON-schema parameters, `enabled_by_default`,
  and legacy execution-mode metadata).
- **`tool.unregister`** *(extension)* — A previously registered tool is
  withdrawn.
- **`tool.request`** *(provider/extension)* — A runtime request to run a
  tool call by id, owner agent id, model-produced name, and CBOR arguments. It
  may come from an agent response or another extension, and can still be
  rejected before any tool provider receives it. Extension-authored
  `call_id`s must be non-empty and globally unique; empty ids or collisions
  with any known live, completed, or durable transcript tool call are refused
  with `harness.notice` only, not a call-id-keyed terminal event. Transcript
  tool-call truth comes from the provider response's `ContextItem::ToolCall`,
  not this routing event. The event is persisted in the session restore log,
  not the agent transcript log, so restore handlers can correlate execution
  state without re-running live tool execution.
- **`tool.started`** *(harness)* — The harness accepted and routed a
  tool request. This runtime broadcast is the signal that the selected tool
  provider should start the call, and that UIs can show a generic pending tool
  line. It intentionally carries no provider-owned display formatting; the tool
  provider owns argument parsing and presentation. The event is persisted in the
  session restore log, not the agent transcript log; live tool execution
  handlers must not run for replayed deliveries.
- **`tool.rejected`** *(harness)* — The harness rejected a tool request
  before any tool provider was asked to run it. UIs can display this as a tool
  call rejection.
- **`tool.result`** *(extension/harness)* — Successful logical runtime tool
  completion, by call id, with tool-owned `result` plus optional UI
  `display` metadata and echoed originator. This event is renderer-facing.
  Provider-only terminal completions use `provider.tool_result` instead.
- **`tool.error`** *(extension)* — Logical tool failure with a message and
  optional structured details. Operational only; transient. Provider-only
  terminal failures use `provider.tool_error` instead.
- **`tool.background_result`** / **`tool.background_error`** *(harness)* —
  Logical notification that a backgrounded tool later completed for real.
  The earlier synthetic placeholder is provider-facing only and is not
  emitted as `tool.result`. Once a call has emitted a background placeholder,
  harness-forced cancellation or teardown also completes it through
  `tool.background_error` (and wait background-completion state), not through a
  second transcript-terminal `tool.cancelled`. Only one real background
  completion is valid for a `call_id`: once either `tool.background_result` or
  `tool.background_error` has been recorded, later background completion events
  for that id are rejected during both live append and durable replay.
- **`tool.progress`** *(extension)* — In-flight progress update with an
  optional message, current/total counters, and/or complete display state.
  Providers should usually emit an initial `tool.progress` immediately after
  receiving `tool.started`, before expensive work, to replace the UI's generic
  pending line with provider-owned formatting.
- **`tool.cancel_request`** *(harness)* — The harness asks an extension to cancel an
  in-flight call.
- **`tool.cancelled`** *(extension/harness)* — A non-backgrounded call was
  cancelled and its foreground transcript tool round is terminal. Operational
  only; transient. Backgrounded calls that already emitted a placeholder must
  use `tool.background_error` for cancellation instead.

## Actions

Action events carry slash-command/action schema and invocation traffic between
extensions, the harness, and interested UIs.

- **`action.schema_published`** — An extension publishes its current action
  schema tree, including command names, descriptions, arguments, and action ids.
- **`action.invoke`** — The harness or UI requests execution of a published
  action by id with CBOR/YAML-compatible arguments and correlation metadata.
- **`action.result`** — The action provider returns a successful result for a
  prior invocation.
- **`action.error`** — The action provider reports an invocation failure with a
  human-readable message and optional structured detail.

## Extensions

Two sub-classes:

### Extension supervision (harness supervisor)

Emitted by the harness's supervisor as it manages child extension
processes.

- **`extension.starting`** — A child extension process is being spawned
  (instance id, name, pid).
- **`extension.ready`** — The extension's `Ready` message was received
  by the supervisor, which synthesizes this bus event so subscribers can
  observe that the extension is fully online.
- **`extension.exited`** — The child process exited; carries exit code
  and/or signal.
- **`extension.restarting`** — The supervisor is restarting an extension
  (attempt counter, optional reason).

### Extension-emitted

Emitted by extensions to advertise capabilities or interact with the
harness/agent.

- **`extension.skill_available`** — The extension discovered a skill on
  disk: name, description, file path, whether to inject it into the
  system prompt, whether users may invoke it with `/skill`, whether model-side
  invocation is disabled (which implies user invocation), and an optional argument hint.
- **`extension.agents_md_available`** — The extension discovered an
  AGENTS.md file and is shipping its contents eagerly so the harness
  can inject them without a tool round-trip.
- **`extension.context_provider_register`** — The extension registers as a
  per-agent context provider that can publish context after
  `session.agent_loaded` and acknowledge with `extension.context_ready`.
- **`extension.session_context_provider_register`** — The extension registers as
  a session-wide context provider that can publish context after
  `session.started` and acknowledge with `extension.session_context_ready`.
- **`extension.context_ready`** — The extension finished publishing
  refreshed prompt context for one loaded agent.
- **`extension.session_context_ready`** — A registered session context provider
  finished publishing refreshed session-wide context such as skills and
  AGENTS.md files after `session.started`.
- **`extension.agent_context_publish`** — The extension publishes context for a
  particular agent/session context provider slot.
- **`extension.prompt_fragment_publish`** — The extension publishes a prompt
  fragment contribution that prompt assembly may include according to config.
- **`extension.prompt_submit_request`** — An extension request to submit a
  user-style or hidden internal prompt to an already loaded agent. The harness
  validates the target agent and, when accepted, publishes the durable
  `agent.prompt_submitted` fact with the requested `message_class`; queued prompts
  that are folded into an in-flight turn preserve the request `ctx_id` on
  `agent.prompt_steered`. Extensions must not forge transcript prompt facts
  directly. `tau-ext-utils` uses internal prompt submissions for timer wakeups.
- **`agent.start_request`** — An extension or harness-owned tool asks
  the harness to start a side/sub-agent conversation: instruction text,
  correlation `query_id`, optional requested `role`, optional tool-call
  attribution, and human-readable task name (used by the `agent_start` tool).
  Tool-backed delegate requests default to `engineer` when `role` is
  absent; non-tool requests without `role` use the currently selected
  interactive role.
- **`agent.start_accepted`** — The harness accepted an agent-start request and
  created or reused the delegated agent route for the requested task.
- **`agent.start_result`** — The agent's final answer to an
  earlier `agent.start_request`, routed point-to-point back to the
  requesting extension. Carries the same `query_id`.
- **`agent.message_sent`** — Harness-owned immutable sender-side projection for
  a short message an agent sent to another agent or to the user. Carries stable
  `message_id`, `sender_id`, recipient (`agent_id`, `external_agent
  { session_id, agent_id }`, or `user`), and `message`.
- **`agent.message_received`** — Harness-owned immutable recipient-side
  projection for an agent-to-agent message. Carries the same stable
  `message_id`, the `sender_id`, optional `sender_session_id` for external
  senders, the receiving `recipient_id`, and `message`; user-recipient messages
  have no received projection. User-recipient sent projections are human-visible
  broadcasts that UIs always render fully in the currently visible transcript. UI
  subscribers filter, summarize, or fully display agent-to-agent message
  projections according to `/set show-messages`. Agent recipients are delivered
  as hidden internal prompts; external senders render as `session/agent`. If a
  side/delegate agent is about to finish, teardown waits until the message turn
  has been dispatched and answered. Interceptors cannot drop or rewrite these
  validated projections. See [agent-messaging.md](agent-messaging.md) for
  model-facing tool examples.
- **`extension.event`** — Custom extension-defined event with an
  extension-owned dotted name and CBOR payload. The nested name must have
  non-empty category and call segments, and must not use reserved first-party
  categories (`tool`, `action`, `agent`, `extension`, `provider`, `harness`,
  `ui`, `shell`, `session`, or `term`). The harness
  routes it like any other event. It is runtime/debug-log state unless a typed
  semantic event is added for a durable use case.

## UI

Emitted by attached UI clients (tau-cli-term, etc.) to express user
intent.

- **`ui.prompt_submitted`** — The user submitted a prompt request for an
  existing agent: session id, text, required `agent_id`, originator (defaults to
  `user`; reused for extension-driven side prompts), and user/internal message
  class. The harness translates accepted requests into durable
  `agent.prompt_submitted` facts.
- **`ui.prompt_draft`** — Trailing-edge debounced (≤1/s) snapshot of the
  current draft buffer. Transient — used for "user is alive" signals
  (e.g. notification idle reset), not persisted. Carries the viewed
  `target_agent_id` when the draft belongs to an existing agent transcript;
  modern producers must set it in that case. Absence means the draft is
  session-level/unscoped, normally the start-new-agent prompt. Legacy peers whose
  payloads predate this field also decode as absent, so future restore/sync
  consumers must not infer the current agent from absence.
- **`ui.focus_changed`** — Attached terminal UI reports focus gained/lost for a
  session when terminal focus events are available. Transient; used for idle and
  notification behavior, not transcript truth.
- **`ui.role_select`** — User requests a role switch. The harness resolves
  the role to a provider-published model at runtime.
- **`ui.agent_model_select`** — User requests a model override for a loaded
  agent. `agent_id` may be omitted only when the harness can unambiguously infer
  the target from session selection/default state.
- **`ui.role_update`** — User changes or deletes a role. Wire actions are
  `delete`, `set_model`, `set_effort`, `set_verbosity`,
  `set_thinking_summary`, `set_service_tier`, `set_compaction_threshold`,
  `set_tools`, `set_enable_tool_groups`, `set_disable_tool_groups`,
  `set_enable_tools`, and `set_disable_tools`. Nullable override setters,
  including `set_tools`, use `null` or omission to clear back to
  model/provider fallback behavior. For `set_tools`, an empty list is an
  explicit empty tool allow-list; the enable/disable vector setters replace
  their corresponding lists, including with empty lists.
- **`ui.detach_request`** — UI is detaching but wants the daemon to keep
  running so a later `tau --attach` can reconnect.
- **`ui.debug_event_stats_request`** — UI debug request for one extension's
  protocol frame I/O counters. The harness consumes it directly and replies only
  to the requesting UI; it is not a durable session fact.
- **`ui.shell_command`** — User submitted a `!` (in-context) or `!!`
  (UI-only) shell command. Carries command id, command, session id,
  `include_in_context` flag.
- **`ui.switch_session`** — User wants to switch to a different session
  in the same daemon, with `new`/`resume` reason.
- **`ui.create_agent`** — UI requests creation of a user-owned agent, optionally
  with the first prompt to append after context loads. The request carries the
  role, initial metadata, optional parent agent, optional prompt correlation id,
  optional `model_override`, and optional `ephemeral`; when present,
  `model_override` is installed on the new agent before its first prompt is
  queued or routed, and `ephemeral: true` keeps the agent transcript and session
  membership memory-only for the daemon lifetime.
- **`ui.tree_request`** — User typed `/tree`: render the selected or targeted
  agent's prompt rewind anchors to chat.
- **`ui.navigate_tree`** — User typed `/tree <anchor>`, `/tree root`, or the
  expert `/tree node <node-id>` form: move the selected or targeted agent head
  to the resolved root-or-node target so the next prompt branches there.
  Prompt anchors are one-based; UI parsers should encode `/tree 0` as the
  explicit root target before the first prompt.
- **`ui.compact_request`** — User typed `/compact`: request provider-side
  compaction for the selected or targeted agent before the next prompt.
- **`ui.cancel_prompt`** — User requests cancellation of a prompt by session,
  optional target agent, and optional prompt id; applies to active or queued
  prompt work when still present.
- **`ui.recall_queued_prompt`** — User requests removing the most recently queued
  prompt from the selected or targeted agent so it can be edited/resubmitted.
- **`ui.set_agent_display_name`** — User requests a durable display-name update
  for a known agent in the session; accepted requests produce
  `agent.display_name_set`.

## Shell (shell extension, user-initiated commands)

Emitted by `tau-ext-shell` (or any extension implementing `!`/`!!`
commands) in response to a `ui.shell_command`.

- **`shell.command_progress`** — A chunk of stdout/stderr from a running
  user-initiated shell command, correlated by `command_id`. Transient.
- **`shell.command_finished`** — A user-initiated shell command exited
  or was cancelled. Echoes session id, command, optional target agent id,
  and `include_in_context` flag from the originating request, plus the
  truncated combined output, exit code, and `cancelled` flag. When
  `include_in_context` is set, the harness injects the output only into the
  validated target agent for that session. A wrong-session, unknown, or
  non-live target is ignored; targetless output goes to the unambiguous current
  user agent, creating one if needed, and ambiguous targetless candidates are
  refused.

## Term (terminal-output side effects)

Targeted at whichever UI is attached and capable of writing escape
sequences to a real terminal. Harness-owned code and extensions may emit these;
the UI is the only consumer. Components without a terminal silently no-op.

- **`term.osc1337_set_user_var`** — Ask the UI to write an iTerm2
  OSC 1337 `SetUserVar` escape sequence. Producers should validate
  names before emitting the event; the terminal UI validates again and
  skips invalid names as defense in depth. The UI base64-encodes the
  value and tmux-wraps if needed. Useful for surfacing notifications,
  build status, or other state to terminal-side tooling.
- **`term.bell`** — Ask the attached terminal UI to ring/flash according to the
  user's terminal settings. It may become a sound, visual flash, desktop
  notification, or no-op.


## Provider repetition stop reason

`provider.response_finished.stop_reason` may be `repetition_detected` when a provider aborts a tight exact streaming loop. Such responses have no tool request, use empty `output_items`, and carry a bounded display-only `error`; clients should treat prior transient deltas as cleared when the preceding status update has `clear_response: true`.
