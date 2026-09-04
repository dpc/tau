# Event log reference

## Durable publication cut

Tau admits each durable semantic frame and complete in-memory projection before
recording or broadcasting it. Admission rejection is invisible to EventLog,
debug JSONL, subscriptions, and continuation consumers. Filesystem durability
then follows asynchronously in one global FIFO; restart observes the longest
valid durable prefix.

## External-message reports and canonical facts

Message bridges publish transient `message.delivered_reported`,
`message.edited_reported`, `message.deleted_reported`,
`message.reaction_added_reported`, `message.reaction_removed_reported`, and
`message.sent_reported` events through ordinary `emit`. These reports use normal
interception and live broadcast. A downstream harness consumer replaces the
claimed publisher with the authenticated extension's stable configured name and
publishes the corresponding immutable durable `message.*` fact. There is no
generic transport registration, admission, reply-routing, or send-completion
service.

Each fact carries a claimed Tau agent target, small universal typed fields, and
bounded opaque `extension_data`. Delivered and sent facts establish
publisher-scoped message IDs; edit, delete, and reaction facts carry opaque
references to a base fact. Generic consumers do not resolve those references or
interpret extension data. Transport authentication, admission, deduplication,
native routing, reply authority, and send/retry policy remain extension-local.

Valid committed canonical incoming facts immediately request one payload-free
live activation and project as escaped `<message event="…">` user context
when branch placement permits.
`message.sent` projects as assistant context and never activates by itself.
Replay reconstructs the same projection without waking the agent or restoring
extension-private authority. A malformed or unavailable target cannot veto
persistence; deterministic projection failures remain visible as bounded
diagnostics without exposing fact text or extension data.

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
  (`critical`, `warning`, `info`, `debug`, or `trace`), and `purpose`
  (`response`, `alert`, or `diagnostic`). Compact UIs show responses and alerts
  and hide diagnostics; verbose UIs apply their notice-level threshold only to
  diagnostics. Critical notices remain defensively visible. Current first-party kinds
  include `extension.config_error`, `extension.optional_skipped`,
  `extension.notice` for harness-authored notices derived from configured-extension
  requests,
  `harness.config_error`, `harness.failure`, `harness.internal_warning`,
  `harness.introduction`,
  `harness.notice`, `harness.replay_error`, `model.selection`,
  `skill.collision`, and `ui.command_error`. Expected skill-name collisions are
  trace-level notices. The `harness.introduction` kind is sent directly to a
  newly spawned harness's initial conversational UI; it is not published,
  persisted, replayed, or broadcast to attached clients.
  Configured extensions request diagnostics with the point-to-point
  `extension_notice_request` message rather than emitting this event. The harness
  caps critical to warning and publishes a live-only `extension.notice` with
  harness-owned source, kind, visibility, and persistence metadata. Add a
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
- **`harness.agent_context_initialized`** — Protected transient current-state
  projection for one exact agent initialization, carrying model-listed skills
  and ordered AGENTS.md path/line/byte summaries.
- **`harness.session_skills_available`** — Protected transient complete
  validated session skill snapshot for role preflight and manual completion.
  Effective skill sources are tagged as file-backed absolute paths or a
  built-in discriminator; built-in content is resolved by skill name and is not
  carried in projection events.
- **`harness.role_selected`** — Which role is currently selected, plus
  the model it resolves to and that model's context-window size if known.
- **`harness.context_usage_changed`** — Updated input/cached token counts
  and percent-of-context-window for the selected role's resolved model,
  after each agent response that reports usage.
- **`harness.agent_context_usage_changed`** — Updated context-usage snapshot for
  a specific agent, used by UIs that render per-agent context pressure.
- **`harness.provider_quota_changed`** — Harness-validated, transient full
  current-state snapshot of bounded account quota windows and exact model-to-pool
  bindings. Observation timestamps are preserved during late-subscriber catch-up.
- **`harness.efforts_available`** — The selected role's resolved model
  reasoning capability: unsupported, fixed, or an exact portable-to-native
  lower-bound mapping.
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
  harness started its bound session. Carries `session_id` and an `initial` or
  `resume` reason.
  Registered session context providers react with per-session setup and reply
  with `extension.session_context_ready`; per-agent context providers react to
  `session.agent_loaded` and reply with `extension.context_ready`. Interceptors
  cannot drop or rewrite it. Exact readiness or disconnect from every
  outstanding registered provider completes session initialization before its
  non-renewable thirty-second cap; provider silence and generic traffic do not
  complete or extend the wait. Final readiness and synchronous harness
  finalization take precedence over deadline classification; absolute expiry
  reports `SessionInitTimeout`, not process `StartupTimeout`.
- **`session.shutdown`** — Must-pass immutable runtime lifecycle fact: the
  harness is leaving the current session, emitted before `session.started` for
  the next one or before final harness teardown. Extensions flush or drop
  per-session state. Interceptors cannot drop or rewrite it.
- **`session.agent_loaded`** — Membership fact: a global agent is loaded into
  this session. The first durable membership transition writes this to the
  session log so resume can fold the loaded-agent set. Restored initialization
  restates already-folded membership transiently with a fresh
  `agent_initialization_id` instead of appending a duplicate membership fact.
  Ephemeral agents set `ephemeral: true`; the fact is
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
  user/internal message class. Its harness-owned `inference_activation` flag
  distinguishes checkpoint-governed work from passive or legacy history;
   steered and injected facts use the same default-false marker. The optional
    harness-owned `internal_kind` marks specialized prompt presentation:
    `context_size_alert` identifies an alert delivery for exact-position live
    and replay UI history, while `background_tool_completion` identifies a
    harness lifecycle notice that human UIs suppress. Harness-stamped
    `submission_source` selects routing and source-aware CLI presentation:
   `Extension { name }` renders once as an attributed message,
   `HarnessInternal` is controlled by the default-off internal-prompt setting,
   and `Legacy` preserves hidden internal behavior. `HumanUi` facts keep raw
    canonical text but project as fieldless `<user>...</user>` context.
    Validated harness-authenticated `trusted_internal_spans` select only the
    text ranges that provider projection frames as `<tau_internal>`; absent
    spans leave all bytes as ordinary payload, regardless of source or text.
- **`agent.prompt_queued`** — A prompt arrived while the agent was busy and was
  queued instead of dispatched. Runtime UI state; not durable transcript truth.
- **`agent.prompt_steered`** — A previously queued prompt folded into an
  in-flight continuation as a steering user message rather than a fresh turn.
  Its immutable harness-owned `inference_activation` marker is true for
   checkpoint-governed work; missing/default-false values are passive or legacy
   and cannot independently wake replay. It carries the same optional
   `internal_kind` delivery tag as `agent.prompt_submitted`
    and requires the same harness-stamped `submission_source` and
    `trusted_internal_spans`. The optional typed
   `self_compaction_terminal` is immutable one-shot delivery authority for self
   `compact`; it contains closed status plus request, call, and optional
   transaction correlation, and duplicate delivery is invalid. Old steered
   records without `submission_source` are unsupported.
- **`agent.user_message_injected`** — Synthetic transcript context inserted by
  the harness (for example shell output) and folded
  like user input. It uses the same immutable harness-owned, default-false
  activation marker: false is passive/legacy context and cannot independently
  wake replay.
- **`agent.initialization_context_set`** — Durable harness-authored replacement
  DTO carrying an optional bootstrap block, frozen effective skills, ordered
  AGENTS.md summaries, and the initialization ID that produced the replacement.
  Every finalization appends one exact-ID replacement, including unchanged
  effective content refreshed during cold restore. The reducer folds the fact
  into replaceable side state without adding a transcript node.
- **`agent.prompt_recalled`** — A queued prompt was recalled for editing.
- **`agent.prompt_rejected`** — Harness-authored transient terminal for one
  ordinary queued prompt that cannot dispatch because provider initialization
  settled with no available models. It carries the owning `agent_id`, the
  user/internal `message_class`, and fixed actionable provider-configuration
  guidance, but no prompt text. It is immutable, must pass interception, and is
  delivered only live; it never enters session or agent journals, semantic
  replay, or provider context. Accepted create-agent initial prompts retain the
  separately correlated `agent.prompt_failed` terminal instead. Because queued
  events have no prompt id, live consumers correlate terminals per agent in FIFO
  broadcast order. Submitted facts remove their matching queue front and retain
  `ctx_id` correlation, so a later `agent.prompt_failed` cannot consume newer
  queued work; otherwise `agent.prompt_failed` or `agent.prompt_rejected`
  consumes the matching user/internal queue front.
- **`agent.prompt_created`** — The harness assembled a provider prompt and
  assigned it an `agent_prompt_id`; payload carries `agent_id`, `session_id`,
  `system_prompt`, materialized `context`, tools or `tools_ref`, model, model
  params, tool choice, originator/provenance, legacy cache-sharing flag,
  optional UI correlation id, optional inline compaction summary, and an explicit
  inference or standalone-compaction operation. First-party
  ChatGPT/Codex cache routing is stable per target agent and does not split on
  those provenance fields. This is
  transient operational delivery state for the selected provider. The full
  payload never enters semantic persistence and is emitted only by the matching
  prompt-start fact's live post-commit continuation. Existing live
  observer/interceptor visibility remains available, but historical replay and
  `GetAgentPromptCreated` do not reconstruct it.
- **`agent.prompt_started`** — Durable, content-free prompt materialization fact.
  Carries the prompt id, agent id, session id, model, captured model parameters,
  owning outer-turn id for ordinary inference, operation, originator, and optional
  UI correlation id. It commits after the durable dispatch owner and
  before the matching transient `agent.prompt_created`. Agent-journal replay
  folds it for exact-one materialization and ordinary-inference generation
  authority, but subscriber catch-up excludes it and replay never recreates
  provider work. UIs and observers should use this when they only need to track
  in-flight prompt state.
- **`agent.outer_turn_started` / `agent.outer_turn_finished`** — Durable
  harness-authored boundaries for one non-overlapping accepted activation. They
  carry the stable turn id and session attribution; the start also identifies the
  initiating transcript occurrence and the finish records its terminal disposition.
  A finish may reference the terminal-owned automatic-compaction decision that it
  makes runnable.
- **`agent.state`** — Transient live runtime snapshot for one agent. Carries
  `agent_id` plus `idle`/`running` state so UIs can show work in progress
  without treating it as transcript history.
- **`agent.watches_updated`** — Transient session-local full snapshot of the
  agents watched by one watcher agent. Empty watched sets are valid after a
  disable; late subscribers receive only current non-empty snapshots.
- **`agent.stats_updated`** — Transient, content-free operational snapshot for
  one loaded agent: binary runtime state, required detailed turn activity,
  current/cumulative tool counters, and latest
  context usage, plus runtime-only self and inclusive authenticated-creator
  subtree estimated equivalent API costs and current canonical work-status
  phase/title. Creator-subtree membership follows only same-session
  `AgentStarted.creator = AgentCreator::Agent`, never metadata
  `parent_agent`; both costs reset on session/runtime reset. A descendant
  increment publishes a complete snapshot for it and each loaded creator
  ancestor. It replaces the old delegation-specific progress stream.
- **`agent.runtime_indicators_declared`** *(Tool/Core extension)* — A bounded,
  transient complete replacement of one configured source's ambient indicators
  for a live agent. The harness unions source contributions and clears them on
  source disconnect, agent unload, and final session shutdown. The only current value
  is `timer_scheduled`; declarations never enter replay.
- **`agent.prompt_terminated`** — A prompt ended without an accepted
  `provider.response_finished` (stale or canceled). For a V1-marked ordinary
  inference owner this is a required exact-prompt agent-journal fact: core folds
  it to release deferred placement without adding provider output. It remains
  excluded from historical subscriber catch-up. Legacy prompt termination stays
  transient. Canceling exact ordinary checkpointed inference also releases its
  runtime dispatch ownership so a later prompt on the same agent can proceed;
  standalone-compaction ownership remains governed by its durable recovery
  contract, and late provider terminals for the canceled id remain discarded.
- **`agent.prompt_failed`** — Transient terminal for an accepted initial prompt
  that failed before `agent.prompt_created` assigned a provider prompt id. It
  carries the create request id, created agent id, prompt `ctx_id`, failure
  stage, and a bounded sanitized message. It is not replayed.
- **`agent.prompt_prewarm_requested`** — Best-effort provider cache prewarm for
  the next prompt prefix. Runtime/provider optimization state.
- **`agent.compaction_triggered`** — Durable manual or harness-scheduled
  inline compaction request. Providers fold it into inline context management;
  standalone compaction instead begins with
  `agent.standalone_compaction_started`.
- **`agent.compacted`** — Durable standalone compaction boundary. Its validated
  ordered replacement window replaces history through its recorded cut while
  the model-visible suffix through `suffix_end` and later branch nodes survive
  exactly once. New boundaries require the transaction id, cut, suffix end,
  compact prompt id, provider-qualified model, and standalone operation as one
  all-present group matching the durable start. Legacy records have all six
  absent and remain hard boundaries. Optional compact-request input/output token
  counts are provider-reported display/accounting only; absent fields preserve
  older journal semantics, and legacy estimate provenance grants no authority.
  Connection ids are intentionally not
  durable. Either form invalidates any previous-response chain.
- **`agent.display_name_set`** — Durable fact that changes an agent's
  human-friendly display name. Carries `agent_id` and the new non-empty display
  name; UIs use it when rendering agent chips and history.
- **`agent.metadata_set_request`** / **`agent.metadata_unset_request`** —
  Transient-by-default mutation requests accepted from configured extensions and
  attached socket UIs. Valid requests produce separate harness-authored canonical
  facts; invalid requests currently have no successor.
- **`agent.metadata_set`** / **`agent.metadata_unset`** — Durable,
  interceptable harness-authored per-agent metadata facts. Keys are strings, values are
  arbitrary CBOR capped at 64 KiB, and `metadata_set` carries an optional opaque
  mutation correlation id plus an `inheritable`
  flag copied to child agents at creation time. Extensions use these facts for
  extension-visible state such as `ext_core-shell_cwd`. A set carrying a
  mutation id is a live commit acknowledgement: interception may rewrite its
  value, but cannot drop it or change its agent, key, correlation id, or
  inheritance flag. Durable state and replay omit the transient mutation id, so
  replay can reconstruct current metadata without impersonating a live commit.
  See
  [`SPEC-agent-metadata-requests-and-canonical-facts`](../specs/SPEC-agent-metadata-requests-and-canonical-facts.md).
- **`agent.started`** — Creation fact for an agent. Durable agents write it to
  their event log; ephemeral agents replay it from memory only. It carries
  optional `parent_agent`; inheritable metadata from that parent is copied into
  the new agent after this fact commits and before the agent is announced loaded.
- **`agent.user_interaction_recorded`** — Content-free durable fact committed
  when a visible user submission is accepted, including a queued submission that
  may later be recalled. The persisted record supplies the acceptance timestamp;
  prompt text is intentionally not duplicated. Interceptors cannot drop,
  replace, or retarget this fact.
- **`agent.head_moved`** — Durable fact that changes an agent's selected tree
  head after navigation, so future prompts branch from the requested root or
  node target.


## Provider models and execution

Configured provider extensions declare model capability; the harness owns accepted
current model state. Provider backends emit execution events for work routed to
their selected models.

- **`provider.models_declared`** — Transient, mutable replacement declaration from
  a configured provider extension. It lists routes with valid local configuration
  and locally usable credentials after hydration, and is replaced when later
  credential observations change; it does not guarantee remote authentication.
  It enters ordinary exact/prefix interception;
  an extension-supplied persistence request is ignored.
- **`provider.models_updated`** — Transient, immutable harness-authored accepted
  current state derived after a model declaration commits. Its
  `publisher_extension_id` identifies the stable configured provider whose complete
  snapshot is replaced; an empty model list withdraws that provider's state. It
  includes the exact route's accepted prompt-input and native tool-result
  modalities; omitted modality metadata means text-only. The harness then publishes
  an optional runtime-only cache contract with documented TTL shape, renewal,
  quota, prefix-version, and privacy facts. The contract contains no cache key,
  object id, prompt content, timestamp, history, or lifecycle state. It remains
  transient current state and is never reconstructed by cold replay. The harness
  publishes `harness.models_available` and related role/model availability events. Current
  state catch-up synthesizes this canonical event for newly attached live
  subscribers; it is not journal persistence or replay and never regenerates the
  declaration.
- **`provider.model_declaration_diagnostic`** — Transient, immutable
  harness-authored rejection of one malformed entry from a committed provider
  declaration. It identifies the stable publisher and model id and lists every
  closed validation issue found in canonical order. Other valid entries from the
  same declaration remain eligible for `provider.models_updated`.
- **`provider.quota_replace_reported`**,
  **`provider.quota_patch_reported`**, and
  **`provider.quota_clear_reported`** — Transient provider-authored account-quota
  observations. Replacements establish or reconcile an opaque profile epoch;
  patches upsert complete stable-key records; clears request removal of only the
  matching epoch. Reports commit through ordinary interception before the
  harness verifies provider/route ownership, bounds, epoch, and sequence.
  Accepted reports derive the canonical current state defined under
  [Harness general](#harness-general); late subscribers never receive raw
  reports.
- **`provider.prompt_submitted_reported`** — Transient Provider-authored acceptance
  observation. It commits before prompt-owner validation.
- **`provider.prompt_submitted`** — Harness-sourced canonical fact for a valid
  acceptance report. Echoes the originator. Transient.
- **`provider.response_updated_reported`** — Transient Provider-authored live response
  observation. It commits before prompt-owner and routing-identity validation.
- **`provider.response_updated`** — Harness-sourced canonical live response update.
  `deltas` carry newly appended displayable assistant/reasoning text. `status`
  carries provider-authored retry/diagnostic status. `compaction` carries
  provider-side compaction lifecycle. `response_stats` carries public
  content-free previous/current response-throughput samples for the current
  provider prompt. Providers count backend response bytes at the transport
  receive boundary before semantic parsing and emit stats at the provider's
  rate-limited cadence. The optional
  `first_semantic_output_elapsed_micros` records provider-observed time from the
  finite attempt's first backend dispatch to accepted semantic output; it is
  captured before batching, repeated unchanged, and remains transient. The
  harness validates prompt ownership and routing, then
  broadcasts these updates unchanged; UI clients render stats directly from this
  event. Stats-only updates are valid and transient.

- **`provider.response_finished_reported`** — Transient Provider-authored terminal
  observation. The committed report enters prompt correlation and the existing
  response terminal pipeline; it never enters semantic replay.
- **`provider.response_finished`** — Harness-sourced durable final assistant output in original
  item order via `output_items`, plus optional response-local usage, provider
  response id, backend metadata, and echoed originator. Terminal request
  rejection may carry a machine-readable `failure_kind`; notably,
  `context_window_exceeded` is independent of bounded display `error` prose.
  Such a rejection may also carry harness-authored `context_limit_telemetry`:
  provider-qualified model and operation, optional provider-reported input
  tokens, optional exact serialized transcript-growth bytes, the advertised
  token window, and a closed observation category. Exact serialized byte growth
  is independent telemetry and is never interpreted as provider token usage.
  Either measurement may be absent independently. The telemetry contains no
  prompt/error/body text and never changes model metadata or thresholds
  automatically. The active explicit threshold and closed policy,
  eligibility flag, and harness action distinguish terminal handling from one
  planned reactive compaction.
  A rejection without nonzero `provider_input_tokens` is classified
  as `insufficient_evidence`; it cannot by itself claim rejection below or above
  the advertised provider limit.
  Usage is absent when every provider counter is unavailable; an explicitly
  reported all-zero usage record remains present zero. Harness-selected
  estimated cost rates and increment are present only with usage, and a
  calculated zero increment remains present zero. The nested cumulative usage
  snapshot is presentation state, not durable accounting input. Optional
  privacy-redacted cache usage carries read/write/miss and token-time storage
  observations, cacheable-prefix and avoided-prefill estimates, refresh reason,
  and expiry confidence, but never prompt content or cache keys. Contradictory
  token classes clamp against total input in read, write, then miss order.
  Successful responses and retryable attempts omit context-limit telemetry. Routed by the harness
  based on the originator.
- **`provider.tool_result`** / **`provider.tool_error`** — Provider-facing
  terminal tool-call completions. These satisfy provider protocol state and
  fold into prompt history, but are not logical UI tool completions. The
  synthetic background placeholder uses `provider.tool_result` only. A validated
  image result may retain its typed bytes in the durable agent transcript and in
  the point-to-point `agent.prompt_created` sent to the selected provider.
   Generic live subscribers receive no image bytes, and historical replay may
   omit provider content entirely; neither generic broadcast nor replay is
   provider-content authority.
   Durable terminals default to `tool_payload`; only harness-stamped
   `harness_dedup_pointer` presentation projects as `<tau_internal>`. Configured
   extensions cannot publish that presentation, and replay/compaction retain it.
  Peer requests routed to harness-internal tools are the explicit exception:
  their loaded-agent correlation is runtime-only, so resulting
  `provider.tool_result` / `provider.tool_error` events remain ownerless and do
  not fold into prompt history.
- **`provider.cache_miss_diagnostic_reported`** — Transient Provider-authored cache
  observation awaiting prompt-owner validation.
- **`provider.cache_miss_diagnostic`** — Harness-sourced canonical diagnostic for a prompt
  with unexpectedly low cache reuse. The harness accepts it only from the
  provider that owns the prompt, and providers emit it before the matching
  `provider.response_finished` closes the pending provider route.
## Tools

Tool events separate extension declarations from harness-owned accepted state,
agent requests, and harness dispatch. The registration lifecycle contract is
[SPEC-tool-declarations-and-canonical-state](../specs/SPEC-tool-declarations-and-canonical-state.md).
Tool specs may classify active calls for detailed turn presentation with the
reserved `tau:turn:manipulator`, `tau:turn:data_fetch`, and `tau:turn:wait`
tags. Calls without one of these tags conservatively classify as manipulator.

- **`tool.registration_declared`** *(Tool/Core extension)* — A tool provider
  proposes a tool
  spec (name, description, JSON-schema parameters, `enabled_by_default`,
  and legacy execution-mode metadata). The declaration is transient and
  interceptable.
- **`tool.unregistration_declared`** *(Tool/Core extension)* — A provider
  proposes withdrawing one of its owned tools. Like registration declarations,
  it is transient and interceptable.
- **`tool.register`** *(harness)* — Protected canonical state for an accepted
  registration, including configured extension and instance provenance. It is a
  transient, immutable, must-pass lifecycle event with no cold-restart replay.
- **`tool.unregister`** *(harness)* — Protected canonical state for an accepted
  active withdrawal. Unknown or non-owner declarations produce a diagnostic
  instead. It has the same transient, immutable, must-pass, no-replay contract
  as `tool.register`.
- **`tool.request`** *(provider/extension)* — A runtime request to run a
  tool call by id, owner agent id, model-produced name, and CBOR arguments. It
  may come from an agent response or another extension, and can still be
  rejected before any tool provider receives it. Extension-authored
  `call_id`s must be non-empty and globally unique; empty ids or collisions
  with any known live, completed, or durable transcript tool call are refused
  with `harness.notice` only, not a call-id-keyed terminal event. Transcript
  tool-call truth comes from the provider response's `ContextItem::ToolCall`,
  not this routing event. With `persist=true`, the event is persisted in the session restore log,
  not the agent transcript log, so restore handlers can correlate execution
  state without re-running live tool execution.
  Configured Provider, Tool, and Core extensions publish the request through
  ordinary generic Emit; it commits before duplicate-call checks or registry
  routing. `Emit.persist=false` keeps it live-only, while `true` records the
  stable configured publisher in session restore history. Historical delivery
  never routes or executes it.
  See [SPEC-tool-requests-and-routing](../specs/SPEC-tool-requests-and-routing.md).
- **`tool.started`** *(harness)* — The harness accepted and routed a
  tool request. This runtime broadcast is the signal that the selected tool
  provider should start the call, and that UIs can show a generic pending tool
  line. It intentionally carries no provider-owned display formatting; the tool
  provider owns argument parsing and presentation. The event is persisted in the
  session restore log, not the agent transcript log; live tool execution
  handlers must not run for replayed deliveries. Its defaulted
  `invocation_policy` is harness-authored hidden policy frozen with the prompt;
  model arguments and extension `tool.request` inputs cannot relax it. Only
  provider calls correlated with a materialized prompt can carry that frozen
  policy. Cooperative peer `tool.request` traffic has no prompt snapshot and
  receives the default empty policy.
- **`tool.rejected`** *(harness)* — The harness rejected a tool request
  before any tool provider was asked to run it. UIs can display this as a tool
  call rejection.
- **`tool.result_reported`** *(Tool/Core extension)* — Transient peer
  observation of successful completion. It commits through ordinary
  interception before the harness validates the captured configured generation,
  exact routed-call owner, tool-call state, media safety, and foreground or
  background policy. Reports may submit optional typed provider content. Only an
  explicitly image-capable tool on an image-capable selected provider route may
  submit typed image content. A committed report is not itself accepted
  completion.
- **`tool.result_display`** *(harness)* — Protected lightweight
  renderer-facing successful completion. It carries call/tool identity, result
  kind, optional generic `display` metadata, and originator, but no raw result or
  provider content. The protected `provider.tool_result` transcript fact retains
  the full validated result. Live publication follows the canonical provider
  commit, and UI replay derives the same display shape from that durable fact.
- **`tool.result`** *(harness)* — Protected transient full-result projection for
  non-UI generic consumers. It retains the tool-owned raw result while clearing
  provider content. Interactive UIs subscribe to `tool.result_display` instead;
  the durable transcript authority remains `provider.tool_result`.
- **`tool.error_reported`** *(Tool/Core extension)* — Transient peer
  observation of logical failure. It has the same generic commit and downstream
  route/generation validation boundary as `tool.result_reported`.
- **`tool.error`** *(harness)* — Protected canonical renderer-facing failure
  with a message and optional structured details. Provider transcript failures
  use `provider.tool_error`. The raw renderer fact remains outside semantic
  history.
- **`tool.background_result`** / **`tool.background_error`** *(harness)* —
  Protected full-result notification for non-UI consumers when a backgrounded
  tool later completes for real. UI clients receive the separate display
  projection below. The earlier synthetic provider placeholder also produces
  `tool.result` and `tool.result_display` projections after its canonical
  `provider.tool_result` commit. Once a call has emitted a background placeholder,
  harness-forced cancellation or teardown also completes it through
  `tool.background_error` (and wait background-completion state), not through a
  second transcript-terminal `tool.cancelled`. Only one real background
  completion is valid for a `call_id`: once either `tool.background_result` or
  `tool.background_error` has been recorded, later background completion events
  for that id are rejected during both live append and durable replay.
- **`tool.background_result_display`** *(harness)* — Lightweight UI projection
  emitted after a canonical `tool.background_result` commit and derived the same
  way during UI replay. It omits the raw successful background output; UI clients
  do not receive `tool.background_result`.
- **`tool.progress_reported`** *(Tool/Core extension)* — Transient peer
  observation of in-flight progress with an optional message, current/total
  counters, and/or complete display state. Tool providers should usually submit
  an initial report immediately after receiving `tool.started`, before expensive
  work. The harness commits and broadcasts the report before validating the
  captured routed-call owner and background state.
- **`tool.progress`** *(harness)* — Protected transient canonical progress. For
  extension-owned calls it derives from a valid committed
  `tool.progress_reported` observation; harness-owned internal tools may publish
  it directly. It uses the harness delivery source and cannot be rewritten or
  dropped.
- **`tool.cancel_request`** *(harness)* — The harness asks an extension to cancel an
  in-flight call.
- **`tool.cancelled_reported`** *(Tool/Core extension)* — Transient peer
  cancellation observation. It commits before downstream generation, route, and
   call-state validation.
- **`tool.cancelled`** *(harness)* — Protected canonical fact that a
  non-backgrounded call was cancelled and its foreground transcript tool round
  is terminal. Backgrounded calls that already emitted a placeholder instead
   derive `tool.background_error`. The canonical event uses the harness source
   and cannot be rewritten or dropped. It carries the same defaulted
   `ToolResultPresentation` discriminator as result and error terminals and an
   optional complete-replacement generic `ToolUseState` display. Background
   cancellation preserves that display on its derived background error.

## Actions

Action events carry command/action schema and invocation traffic between
extensions, the harness, and interested UIs.

- **`action.schema_declared`** — An Action-capable configured extension declares
  a complete replacement schema snapshot.
- **`action.schema_published`** — The harness publishes an accepted current
  schema stamped with its logical owner.
- **`action.invoke`** — The harness or UI requests execution of a published
  action by id with CBOR/YAML-compatible arguments and correlation metadata.
- **`action.result_reported`** — The provider reports successful output for
  downstream exact-owner correlation.
- **`action.result`** — The harness returns validated output only to the
  requesting UI.
- **`action.error_reported`** — The provider reports a failure for downstream
  exact-owner correlation.
- **`action.error`** — The harness returns a validated or synthesized failure
  only to the requesting UI.

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

Every authenticated configured extension kind may author session-discovery
registration, complete source snapshots, and readiness without a capability;
unconfigured/socket peers may not. Raw events default to `persist=false`, never
enter semantic journals or replay, and commit before projection or wait release.
See
[`SPEC-session-discovery-declarations-and-readiness`](../specs/SPEC-session-discovery-declarations-and-readiness.md).

Every authenticated configured extension kind may also author per-agent
discovery and context events without a capability; unconfigured/socket peers may
not. Registration controls captured wait participation. Values and readiness
must match the current session, loaded agent, and initialization id; per-agent
readiness never releases session initialization. Raw events are
transient runtime observations and never enter semantic replay. See
[`SPEC-per-agent-context-declarations-and-readiness`](../specs/SPEC-per-agent-context-declarations-and-readiness.md).

- **`extension.session_discovery_snapshot_declared`** — A complete transient
  session-baseline source snapshot containing skill candidates and ordered
  AGENTS.md files. Optional sampled modification times are signed microseconds
  from the Unix epoch, so negative values represent pre-epoch files. Empty
  lists atomically clear that source.
- **`extension.agent_discovery_snapshot_declared`** — A complete transient
  source snapshot for one exact session, agent, and
  `agent_initialization_id`. Empty lists atomically clear that pending source.
- **`extension.context_provider_register`** — A transient declaration that commits
  before the extension registers as a
  per-agent context provider that can publish context after
  `session.agent_loaded` and acknowledge with `extension.context_ready`.
- **`extension.session_context_provider_register`** — The extension registers as
  a session-wide context provider that can publish context after
  `session.started` and acknowledge with `extension.session_context_ready`. Registration
  is a transient declaration and commits before provider membership changes.
- **`extension.context_ready`** — A transient acknowledgement that commits before
  releasing the extension's wait for refreshed prompt context for one agent.
- **`extension.session_context_ready`** — An extension acknowledges that it finished
  publishing refreshed session-wide context such as skills and AGENTS.md files after
  `session.started`. The transient acknowledgement commits before it can release session
  initialization; only an effective registered waiter can release the barrier.
- **`extension.agent_context_publish`** — A transient value publication that
  commits before replacing the extension's contribution for a particular
  agent/context key. Registration and loaded-agent membership do not gate raw
  publication.
- **`extension.prompt_fragment_publish`** — The extension publishes a
  prompt-fragment declaration that defaults to transient. Every authenticated
  configured extension kind may publish one. It commits through ordinary
  interception before the harness replaces the source/name prompt-assembly slot;
  declarations are not replayed or synthesized for late subscribers.
- **`extension.internal_prompt_submit_request`** — A narrow extension request to
   submit internal model-classified text to an already loaded agent. Every
  authenticated configured extension kind may publish it; unconfigured/socket
  peers may not. The transient request commits before the harness validates the
   exact live publisher generation and target agent and, when accepted, publishes
   one internal `agent.prompt_submitted` or `agent.prompt_steered` fact stamped
   with the authenticated configured extension name; queued prompts preserve the
   request `ctx_id`. It has no user-message class. Optional typed provenance is
   absent/`internal_prompt` for ordinary
  internal prompts and `timer` for timer wakeups; it grants no additional
  authority and is copied only into content-free `agent.activation_queued`
  classification. The request remains transient, and replay/recovery never
  resubmits it. `tau-ext-utils` uses explicit `timer` provenance. External user messages instead
  enter through bridge `message.delivered_reported` events and the resulting
  canonical `message.delivered` facts.
  See [`SPEC-internal-prompt-submit-requests`](../specs/SPEC-internal-prompt-submit-requests.md).
- **`agent.start_request`** — An extension or harness-owned tool asks
  the harness to start a side/sub-agent conversation: instruction text,
  correlation `query_id`, optional requested `role`, optional tool-call
  attribution, and optional extension-supplied human-readable task name.
  Configured extension requests default to `persist=false` and commit through ordinary
  interception before role/parent validation, duplicate route rebinding,
  acceptance/rejection, or child creation. Unconfigured and socket peers may not
  publish them; stale connection or session generations are observation-only.
  Raw requests never enter semantic history.
  Tool-backed delegate requests default to `engineer` when `role` is
  absent; non-tool requests without `role` use the currently selected
  interactive role.
  See [`SPEC-start-agent-requests`](../specs/SPEC-start-agent-requests.md).
- **`agent.start_accepted`** — Post-commit acknowledgement that the harness
  reserved one `start_id` and `agent_id` and now owes either the matching
  inference-dispatch checkpoint or `agent.start_failed`. It does not claim that
  a route, membership, prompt, or provider dispatch exists yet.
- **`agent.start_failed`** — Compact transient canonical terminal for one
  post-accept startup failure. It carries `start_id`, `agent_id`, the failed
  phase, and a stable reason; the requester-directed `agent.start_result` error
  is projected only after this event commits.
- **`agent.start_result`** — The agent's final answer to an
  earlier `agent.start_request`, routed point-to-point back to the
  requesting extension. Carries the same `query_id`.
- **`agent.message_sent`** — Harness-owned immutable sender-side projection for
  a short message an agent sent to another agent. Carries stable `message_id`,
  `sender_id`, recipient (`agent_id` or
  `external_agent { session_id, agent_id }`), and `message`.
- **`agent.message_received`** — Harness-owned immutable recipient-side
  projection for an agent-to-agent message. Carries the same stable
  `message_id`, the `sender_id`, optional `sender_session_id` for external
  senders, the receiving `recipient_id`, and `message`. UI subscribers filter,
  summarize, or fully display agent-to-agent message projections according to
  `:set show-messages`. The received projection is the sole model payload; local
  senders render in a stable-sender-labelled escaped outer `<tau_internal>`
  envelope and external senders in an authenticated peer envelope. Live
  activation is a payload-free runtime wake; replay restores context without
  waking. If a side/delegate agent is about to finish, teardown waits until the
  message turn has been dispatched and answered. Interceptors cannot drop or
  rewrite these
   validated projections. Agent display names are deliberately absent from these
   semantic projection fields: UIs may supplement ids from authoritative folded
   metadata without changing message content or routing identity. See
   [agent-messaging.md](agent-messaging.md) for model-facing tool examples.
  Unexpected watched-agent unload also uses this event with
  `kind=watch_lifecycle`, an exactly empty `message`, and the matching structured
  `watch_lifecycle { state, reason }` payload. The payload is present if and only
  if the kind is `watch_lifecycle`.
- **`extension.event`** — Custom extension-defined event with an
  extension-owned dotted name and CBOR payload. The nested name must have
  non-empty category and call segments, and must not use reserved first-party
  categories (`tool`, `action`, `agent`, `extension`, `provider`, `harness`,
  `ui`, `shell`, `session`, or `term`). Every authenticated live configured
  extension kind and attached local UI may publish one; unconfigured,
  disconnected, non-UI socket, and dedicated external-message peers may not.
  The harness commits it through ordinary interception for exact/prefix live
  subscribers. It remains runtime/debug-log state for either `persist` value and
  has no semantic or historical replay unless a separately approved typed event
  is added. Wire delivery does not yet carry authenticated publisher identity.
  See [`SPEC-custom-extension-events`](../specs/SPEC-custom-extension-events.md).

## UI

Emitted by attached UI clients (tau-cli-term, etc.) to express user
intent.

- **`ui.prompt_submitted`** — The user submitted a prompt request for an
  existing agent: session id, text, required `agent_id`, originator (defaults to
  `user`), and user/internal message class. For an authenticated visible-user
  request, successful admission durably records the interaction, makes the exact
  existing target `active`, broadcasts fresh complete stats, and then queues or
  dispatches the prompt. The required id is the target; textual mentions do not
  route. Other accepted requests do not mutate navigation. The harness translates
  accepted requests into durable `agent.prompt_submitted` facts.
- **`ui.prompt_draft`** — A first snapshot is emitted immediately, then later
  edits are coalesced to at most one snapshot per second. The event defaults to
  transient and is used for "user is alive" signals (e.g. notification idle
  reset). The interactive CLI explicitly sends `persist=true`, so accepted
  drafts receive run-local sequencing, live broadcast, and best-effort
  `events.jsonl` publication, but the harness excludes them from semantic
  stores and historical replay for either persistence value. The CLI omits
  prompt content by default;
  set `send_prompt_draft_content: true` in `cli.yaml` or a `cli.d` layer to
  include the full current buffer, except that a buffer recognizable as
  `:email auth google finish ...`—including the one-colon escaped spelling
  `::email auth google finish ...`—is published as exactly `:email auth google
  finish <redacted>`. Carries the viewed
  `target_agent_id` when the draft belongs to an existing agent transcript;
  modern producers must set it in that case. Absence means the draft is
  session-level/unscoped, normally the start-new-agent prompt. Legacy peers whose
  payloads predate this field also decode as absent, so future restore/sync
  consumers must not infer the current agent from absence. Only an attached
  socket UI may publish one.
- **`ui.focus_changed`** — Attached terminal UI reports focus gained/lost for a
  session when terminal focus events are available. It is a live subscriber
  observation, not transcript truth; Tau currently has no first-party focus
  subscriber. It defaults to transient, while the CLI currently sends
  `persist=true`. It has the same authority and no-store contract as prompt drafts.
  See
  [`SPEC-ui-prompt-draft-and-focus-events`](../specs/SPEC-ui-prompt-draft-and-focus-events.md).
- **`ui.role_select`** — User requests a role switch. The harness resolves
  the role to a provider-published model at runtime.
- **`ui.agent_model_select`** — User requests a model override for a loaded
  agent. `agent_id` may be omitted only when the harness can unambiguously infer
  the target from session selection/default state.
- **`ui.role_update`** — User changes or deletes a role. Wire actions are
   `delete`, `set_model`, `set_effort`, `adjust_effort`, `set_verbosity`,
   `adjust_verbosity`, `set_thinking_summary`, `adjust_thinking_summary`,
   `set_service_tier`, `set_compaction_threshold`,
  `set_tools`, `set_enable_tool_groups`, `set_disable_tool_groups`,
  `set_enable_tools`, and `set_disable_tools`. Nullable override setters,
  including `set_tools`, use `null` or omission to clear back to
  model/provider fallback behavior. For `set_tools`, an empty list is an
   explicit empty tool allow-list; the enable/disable vector setters replace
   their corresponding lists, including with empty lists.
   Each `adjust_*` action carries a positive `increase` or `decrease` amount,
   resolves from the role's current effective model value, and saturates at the
   setting's lowest or highest level before storing an absolute override.
- **`ui.shell_command`** — User submitted a `!` (in-context) or `!!`
  (UI-only) shell command. Carries command id, command, session id,
  `include_in_context` flag. On attach, a UI may receive replay-marked
  current-state snapshots for at most 128 routes in ascending public-id order
  and within a 64 KiB aggregate CBOR-encoded-payload budget. These snapshot
  bounds do not cap or reject live routing. A route exceeding the
  remaining byte budget is skipped while later smaller routes remain eligible;
  one UI-only replay notice reports the omitted count.
  The notice requires exact subscriptions to both `ui.shell_command` and
  `harness.notice`. Omitted routes continue live, and their completion renders
  as a standalone terminal.
  Snapshots carry no progress or output and remain UI-only and non-durable,
  including for `!!` and ephemeral targets; the later live completion settles
  the same correlated row.
- **`ui.create_agent`** — UI requests creation of a user-owned agent, optionally
  with the first prompt to append after context loads. The request carries the
  request correlation id, role, initial metadata, optional parent agent, optional prompt correlation id,
  optional `model_override`, and optional `ephemeral`; when present,
  `model_override` is installed on the new agent before its first prompt is
  queued or routed, and `ephemeral: true` keeps the agent transcript and session
  membership memory-only for the daemon lifetime.
- **`ui.create_agent_result`** — Transient requester-directed terminal admission
  result for `ui.create_agent`. It echoes the request and session ids and reports
  either the created agent plus `Absent`/`Queued` initial-prompt admission state
  or a categorized rejection with an optional already-created agent id. `Queued`
  means preprocessing accepted the prompt; it does not assert preprocessing,
  provider submission, or execution success. It is never replayed.
  See
  [`SPEC-ui-create-agent-admission`](../specs/SPEC-ui-create-agent-admission.md).
- **`ui.navigate_tree`** — User typed `:tree <anchor>`, `:tree root`, or the
  expert `:tree node <node-id>` form: move the selected or targeted agent head
  to the resolved root-or-node target so the next prompt branches there.
  Prompt anchors are one-based; UI parsers should encode `:tree 0` as the
  explicit root target before the first prompt.
- **`ui.compact_request`** — User typed `:compact`: request provider-side
  compaction for the selected or targeted agent before the next prompt. A busy
  target now receives `compaction queued` after the target journal records the
  pinned durable intent. The harness claims it at the first provider-closed
  boundary before later ordinary inference. Repeated requests coalesce. A sole
  installed harness-owned wait is still cancelled eagerly so its complete round
  closes before compaction.
- **`ui.cancel_prompt`** — User requests cancellation of a prompt by session,
  optional target agent, and optional prompt id; applies to active or queued
  prompt work when still present.
- **`ui.recall_queued_prompt`** — User requests removing the most recently queued
  prompt from the selected or targeted agent so it can be edited/resubmitted.
- **`ui.set_agent_display_name`** — User requests a durable display-name update
  for a known agent in the session; accepted requests produce
  `agent.display_name_set`.

## Shell (shell extension, user-initiated commands)

`tau-ext-shell` (or another registered Tool/Core shell provider) reports
progress and completion in response to a `ui.shell_command`; the harness
publishes the canonical UI-facing facts.

The harness routes each request point-to-point to exactly one registered generic
shell instance. Zero instances fail the command; multiple instances fail as
ambiguous until an explicit selection mechanism exists. Targetless commands are
resolved to one current-session user agent before delivery, and stale-session
commands fail without execution. The originating command is projected to all
attached UIs so progress and the single terminal result have a display block.
The selected extension snapshots that target agent's per-instance workdir at
admission. The harness records the selected provider and canonical request
identity. It accepts progress and exactly one terminal event only from that
provider with matching immutable fields. Stale, duplicate, non-owner, or altered
reports remain committed observations but are rejected for canonicalization.
Provider disconnect and session shutdown consume pending routes with a
harness-owned terminal failure. UI command ids must be non-empty,
at most 256 bytes, and unique while in flight; invalid or concurrent duplicate
ids are rejected before projection. For provider execution, the harness replaces
the UI id with a fresh internal route id and maps accepted progress/terminal
events back to the UI id. A completed UI id may therefore be reused without a
delayed provider event attaching to its newer route. The UI id remains reserved
until its mapped or harness-owned terminal finishes interception and commits, so
a parked older terminal cannot finalize a newly projected block.
Interception may rewrite progress chunk/stream payload fields, but
progress/terminal correlation and target identity remain harness-owned. A
validated terminal is immutable and must-pass so its UI projection and optional
transcript injection cannot diverge.

- **`shell.command_progress_reported`** — A peer-authored chunk of stdout/stderr
  from a running user-initiated shell command. The harness commits the report
  before validating the private provider route. Invalid, stale, or non-owning
  reports remain committed observations but produce no canonical fact.
  Transient and never stored.
- **`shell.command_progress`** — Harness-authored canonical progress, mapped to
  the UI lifecycle `command_id`. Transient.
- **`shell.command_finished_reported`** — A peer-authored terminal observation
  carrying the private provider route id and echoed request identity. The
  harness commits it before exact generation, ownership, and identity validation.
  Invalid reports remain committed observations but do not consume a route.
  Transient and never stored.
- **`shell.command_finished`** — A harness-authored user-initiated shell command exited
  or was cancelled. Echoes session id, command, optional target agent id,
  and `include_in_context` flag from the originating request, plus the
  truncated combined output, exit code, and `cancelled` flag. When
  `include_in_context` is set, the harness stores this self-contained event in
  the non-ephemeral target agent journal and injects the output only into the
  harness-recorded target agent for that session after the canonical completion
  commits. UI-only `!!` completions remain transient. See
  [`SPEC-shell-command-reports-and-canonical-facts`](../specs/SPEC-shell-command-reports-and-canonical-facts.md).

## Term (terminal-output side effects)

Targeted at whichever UI is attached and capable of writing escape
sequences to a real terminal. Harness-owned code, authenticated configured
extensions of any kind, and attached local UIs may emit these; unconfigured,
disconnected, and dedicated external-message peers may not. They cross ordinary
interception and commit but never enter semantic history. Terminal UIs subscribe
live-only and reject replay-marked delivery before acting. Components without a
terminal silently no-op. See
[`SPEC-terminal-output-side-effect-events`](../specs/SPEC-terminal-output-side-effect-events.md).

- **`term.osc1337_set_user_var`** — Ask the UI to write an iTerm2
  OSC 1337 `SetUserVar` escape sequence. Producers should validate
  names before emitting the event; the terminal UI validates again and
  skips invalid names as defense in depth. The UI base64-encodes the
  value and tmux-wraps if needed. Useful for surfacing notifications,
  build status, or other state to terminal-side tooling.
- **`term.bell`** — Ask the attached terminal UI to ring/flash according to the
  user's terminal settings. It may become a sound, visual flash, desktop
  notification, or no-op.


## Standalone compaction control

- **`agent.standalone_compaction_started`** — Harness-owned durable transaction
  start. Newly emitted starts capture a provider-valid closed branch cut,
  optional resume watermark, pre-minted
  compact prompt id, provider-qualified model, standalone operation, originator,
  and explicit retry predecessor. A cut never separates a tool-calling assistant
  response from its complete terminal results node. A successor may preserve or
  retreat along the failed cut's ancestor path, but cannot advance, cross
  branches, or replace an existing resume watermark with a sibling branch. New
  successful boundaries repeat the prompt/model/operation tuple for replay
  validation. Historical failed open-prefix starts remain replay-valid only for
  explicit recovery by a normalized successor; their immutable events are not
  rewritten.
- **`agent.standalone_compaction_failed`** — Harness-owned terminal transaction
  failure with a safe categorical reason and retained resume obligation. A
  public Responses output-limit failure also retains its typed partial output,
  usage, response id, attempt, and backend in a non-context projection; replay
  requires the matching `output_length_exceeded` reason and never treats that
  projection as a replacement window. Raw provider
  diagnostics are deliberately excluded.
- **`agent.inference_dispatch_started`** — Durable checkpoint committed before
  provider inference dispatch. Its `through` head acknowledges only activation
  nodes represented by that immutable prompt snapshot. A checkpoint without a
  matching durable terminal provider response restores as dispatch-uncertain;
  it is not automatically resent.
- **`agent.tool_dispatch_observed`**, **`agent.tool_backgrounded_observed`**,
  **`agent.tool_wait_observed`**, **`agent.tool_wait_registered`**, **`agent.activation_queued`**,
  **`agent.tool_wait_settled`**, **`agent.tool_cancellation_requested`**, and
  **`agent.tool_terminal_classified`** — Content-free, best-effort runtime
  observations linked by opaque random observation IDs and exact declaration
  references. They validate and attempt bounded nonblocking admission of one
  best-effort journal frame, but do not wait for filesystem I/O or acknowledgement,
  traverse interception, deliver to subscribers, or make runtime outcomes
  depend on append success. Replay is observation-only. A crash, append failure, or selected
  snapshot may leave references unresolved or calls incomplete; consumers must
  not reconstruct missing edges from payload prose, time, or journal adjacency.

## Provider repetition stop reason

`provider.response_finished.stop_reason` may be `repetition_detected` when a provider aborts a tight exact streaming loop. Such responses have no tool request, use empty `output_items`, and carry a bounded display-only `error`; clients should treat prior transient deltas as cleared when the preceding status update has `clear_response: true`.

## Watched provider status

`provider.response_updated.status.retry` carries structured retry facts independently of human display text. The harness projects current retry and terminal state as `agent.message_received` with `kind=watch_provider_status`. Its nested `state` is tagged by `phase`; variant-specific required fields prevent retry, recovery, blocked, uncertain, and terminal shapes from being mixed. `recovering_context` is reserved for reactive compaction. The nonzero `agent_watch_retry_notification_threshold` harness setting inclusively suppresses live `retrying` delivery through its configured attempt. Above it, the first occurrence of each retry category produces a live model notification; later same-category attempts only refresh the current snapshot. Zero disables threshold suppression. Per watch subscription, turn generation, and provider prompt, `recovering_context` is delivered once, while each sanitized `blocked` and `dispatch_uncertain` category is delivered once. Terminal failure is always delivered. Initial late-watch snapshots are client-visible but non-prompt; historical attempts are not replayed.

`agent.message_received` also supports `watch_work_status`, `watch_long_wait`,
and `watch_lifecycle`. Work status carries a closed phase, runtime-local status
epoch, optional canonical title of at most 160 UTF-8 bytes, and initial-snapshot
marker. Long-wait delivery carries the work epoch and newly crossed whole-minute
threshold; it is never a late-watch historical snapshot. The harness accumulates
the union of actual monotonic installed-wait intervals within one Working epoch
and emits thresholds at 15, 30, 60, 120, 240, 360, then every 120 minutes.
Current runtime accounting, monotonic timestamps, and crossed-threshold cursors
are not restored; already committed recipient projections remain durable replay
context without restarting timers or live fanout.
Lifecycle delivery is content-free and reports a stopped watched endpoint with
the closed reason `unexpected_unload` or `restored_delegation_route_lost`.

### Reactive context recovery fields

`agent.inference_dispatch_started` optionally records the provider-qualified
`model`, `operation`, and immutable pre-activation `activation_cut`; legacy
records omit these and cannot authorize automatic recovery.
`provider.response_finished.recovery_disposition` is harness-authored, defaults
to `none`, and is `reactive_compaction_planned` only for a canonical no-output
ordinary-inference context rejection.
`agent.standalone_compaction_started.trigger` defaults to `manual`.
`automatic_threshold_evidence` carries the uniquely claimed newest ancestral
provider prompt, its nonzero reported input count, the exact crossed threshold,
and threshold source. Legacy `automatic_threshold` decodes but supplies no new
authority. `automatic_policy` uniquely claims a terminal-owned eager decision,
whose evidence carries the same exact fields. `reactive_context_overflow`
carries the failed inference prompt id and claims that planned recovery.
A typed standalone context rejection may commit a `context_retreat` successor
plan; its `automatic_context_retreat` start must claim that exact plan and strict
previous provider-closed cut. `automatic_continuation` links only a successful
rejection-authorized forward-rolling pass to its immediate durable predecessor.
`automatic_preflight_failure` commits a typed no-provider-dispatch byte-budget or
route terminal under its correlated authority.

`provider.response_finished.automatic_compaction_decision` and the corresponding
field on a harness-authored canceled `agent.prompt_terminated` are optional
harness authority. New decisions carry one coalesced eager action identity,
outer-turn identity, resolved model, effective threshold, and exact provider
evidence. Evidence-less legacy decisions remain decodable but cannot start
provider work. A response's assistant node, or a cancellation terminal's parent,
is the cut. The matching outer-turn finish repeats the action identity before a
protected standalone start may claim it.

`provider.response_finished.output_length_disposition` is also harness-authored
and defaults to `none`. An eligible reasoning-only `length` terminal may carry
one `continuation_planned` reservation for its current consecutive
reasoning-only run; its successor checkpoint carries the matching
`output_length_continuation` owner and remains in the same outer turn. A
committed successful selected-branch ordinary response with an accepted
canonical tool-call round rearms the budget, so later runs in that outer turn
may carry another plan with the same fixed ordinal and limit but different
source and successor prompt ids.
The successor response carries `continuation_terminal`. Every `length` terminal
is semantically incomplete. Watch projection uses category `output_length` and
sticky state `terminal_incomplete`; late subscribers see that state, while final
assistant and successful worker notifications remain suppressed.
Clients must display or report incomplete retained reasoning or partial prose
rather than treating it as a successful final answer.
`provider_attempt` is also harness-authored. It defaults to one and records the
finite transport attempt that produced this terminal, independently of the
continuation ordinal. Cold watch restore reads this durable value.

A reserved successor rejected for context carries recovery disposition only.
Its exact post-compaction descendant remains in the lineage. If selection moves
away instead, Tau closes the dormant original branch with steer, owner, failure,
and finish facts without dispatching the successor; the selected sibling receives
only a visible notice. This synthetic failure applies only before the reserved
successor prompt-start commits. After prompt-start, the dispatched owner supplies
the sole terminal even if selection moves elsewhere.
- **`agent.manual_compaction_requested`** — harness-owned durable acceptance of
  UI `:compact` or a model-callable `compact`/`agent_compact` request, including bounded
  request/caller/target/prompt/tool-call/model correlation.
- **`agent.manual_compaction_request_failed`** — exactly one categorical
  terminal outcome when an accepted request cannot reach standalone transaction
  start.
- **`agent.manual_compaction_request_satisfied`** — a successful automatic
  transaction consumed a queued UI intent, avoiding a redundant manual provider
  call.
- **`agent.standalone_compaction_started`** — standalone transaction start; a
  `manual_agent_tool` trigger carries model-tool correlation, while `manual_ui`
  carries the durable queued UI request id.
- **`ui.retry_prompt`** — Correlated request for the harness to resolve the
  selected agent's exact in-flight prompt and direct a manual delayed-retry
  control to its owning provider.
- **`provider.retry_prompt_result_reported`** — Owning provider scheduler's transient
  correlated `accepted` or `not_parked` report for that exact prompt. A valid report
  produces only the requester-directed harness-sourced UI outcome.
- **`ui.retry_prompt_result`** — Requester-directed retry outcome, including
  harness-side validation failures and the captured target-agent label.

## Shared agent navigation mode

`agent.stats_updated` is a transient, must-pass, immutable complete operational
snapshot for one loaded agent. Its required `navigation_mode` is independent of
`runtime_state`; its required detailed turn activity reduces response generation,
active manipulator or uncategorized calls, data-fetch calls, wait calls,
scheduled timers, and idle in that precedence order. It also carries the current
canonical work-status phase/title,
self-only `estimated_api_cost`, and inclusive
`creator_subtree_estimated_api_cost`. The subtree field defaults to zero when
decoding older peers and, like every stats field, is transient rather than
journaled.
Current snapshots are delivered during catch-up before replay completion.

UI clients request absolute `set_active`, `set_active_auto`, or `set_suspended`
writes with transient `ui.set_agent_navigation_mode`. The harness validates the
current session and loaded membership, broadcasts a fresh stats snapshot after
accepted writes, and directs `ui.set_agent_navigation_mode_result` only to the
requester. Event-loop order is last-accepted-write-wins. Results are diagnostics,
not cache updates, and have no ordering guarantee relative to snapshots.

An authenticated visible human prompt successfully admitted to an existing
loaded target is an implicit absolute `set_active` write. After the durable
content-free interaction marker succeeds, the harness broadcasts a fresh
complete stats snapshot before queue or dispatch; it does not synthesize a
navigation result. Rejected, internal, extension-originated, or unauthorized
requests do not write. Queue promotion, steering, and replay do not repeat the
write. Same-daemon reconnect sees the current classification; cold restore
recomputes defaults. UIs therefore update navigation only from complete stats,
never optimistically from outgoing or replayed prompt events.

An authenticated bare inter-session message that creates a peer-entrypoint
recipient performs the same runtime-only `set_active` classification after
durable creation and current-session membership setup, and broadcasts complete
stats. This applies only to the newly created recipient; existing bare or exact
recipients and other start paths retain their existing navigation behavior.
- **`agent.cache_refresh_requested`** — Harness-authored, transient, sensitive
  cache-refresh request sent only to one captured Provider connection.
- **`agent.cache_refresh_cancel_requested`** — Harness-authored, transient,
  directed cancellation for one refresh id.
- **`provider.cache_refresh_finished_reported`** — Provider-authored transient,
  content-free terminal observation.
- **`provider.cache_refresh_finished`** — Harness-canonical transient,
  content-free terminal after exact owner correlation.
- **`provider.standalone_execution_accounted`** — Harness-sourced durable
  response-local accounting for one standalone backend attempt. Its required
  session id selects the live and restored ledger; the prompt id and logical
  attempt form its idempotency key. Usage is explicitly known or unknown, and
  output acceptance is independent from billing. Standalone retry attempts are
  bounded at 64, with attempt 65 reserved for the terminal; larger retry statuses
  remain transient report diagnostics and do not update accounting or watcher
  attempt state.
- **`provider.standalone_execution_accounting_corrected`** — Harness-sourced
  durable final correction for one cancellation-time awaiting observation. It
  keeps the same logical attempt, counts no request, and finalizes only the
  previously absent usage, backend, and cost from the same live provider
  generation.
