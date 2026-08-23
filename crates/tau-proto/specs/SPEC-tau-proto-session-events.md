# SPEC-tau-proto-session-events: Session events

## Record justification

Session-facing event contracts span protocol DTOs, serde names, `EventName`
constants, harness routing and persistence, and multiple client projections, so
no one implementation area can describe their shared wire and lifecycle invariants.

## Event names and routing

`Event` serde `rename` values, `EventName` constants, and `Event::name()` are one contract. When adding or renaming an event, update all three together and update `docs/events.md` when the selected guide should mention the event.

First-party event categories (`tool`, `action`, `agent`, `extension`, `provider`, `harness`, `ui`, `shell`, `session`, and `term`) are reserved for typed protocol events. `CustomEvent` names must use extension-owned categories so extension payloads cannot spoof first-party routing or policy keys.
Parsed event names and custom event payload names must have non-empty category and call segments; empty segments are malformed protocol data rather than extension-owned names.

`provider.response_updated.response_stats` is public provider-owned response-liveness metadata. Providers submit it on `provider.response_updated_reported`; the harness preserves it on the correlated canonical update. It is content-free, prompt-local, and owned by the provider because the provider owns the backend request lifecycle and reads the response byte stream. Providers attach previous/current cumulative samples to rate-limited response updates: `previous` is the last sample that was actually emitted for that provider prompt, and `current` is the new cumulative sample. Providers may emit the first non-empty response/progress/stat update immediately so UIs learn that output has started. Later non-terminal provider response/progress/stat updates must not be emitted more than once per second per prompt; later byte changes never bypass that cadence. A final flush may bypass the cadence immediately before the provider prompt closes.

After `provider.response_updated_reported` commits, the harness validates its
captured Provider source and prompt routing, then preserves the stats on canonical
`provider.response_updated`; it must not consume, strip, remap, account, or
separately project them. UI clients render response throughput from the canonical
update. Stats-only reports remain valid public transient updates after
canonicalization.

## Tree navigation targets

UI tree navigation is protocol-modeled in user-facing terms. The default
`ui.navigate_tree` target is a one-based prompt anchor, not a raw transcript
node id; `0`/before-first is represented as an explicit root target; and raw
node navigation is reserved for an explicit node target. Durable
`agent.head_moved` records the resolved root-or-node branch head, so replay can
restore both ordinary node heads and the root cursor.

## Output-length continuation

`provider.response_finished.output_length_disposition` is a defaulted,
harness-authored durable disposition. `continuation_planned` carries the outer
turn, pre-minted successor prompt, and the fixed `ordinal=1, limit=1` for one
consecutive reasoning-only run. Multiple plans may share an outer turn after a
committed selected-branch tool-call response rearms the budget; source and
successor prompt ids distinguish those lineages.
`continuation_terminal` carries the outer turn, source prompt, fixed ordinal,
completed/incomplete/failed/cancelled outcome, and the explicit
`outer_turn_finish_owed` crash-repair authority.

`agent.inference_dispatch_started.output_length_continuation` binds the reserved
successor to its source prompt, outer turn, and ordinal.
`agent.prompt_steered.internal_kind=output_length_continuation` identifies the
sole harness-authored continuation instruction. Watchers use provider category
`output_length` and state `terminal_incomplete`; neither state is a successful
final response.

`provider.response_finished.provider_attempt` defaults to one and is omitted at
that value. A non-default value is the harness-authored finite transport attempt
that produced the terminal response, independent of the continuation ordinal.

If selection moves to a sibling after a plan commits but before the reserved
successor prompt starts, the original branch closes with its exact steer,
reserved owner, pre-start harness `failed` terminal, and stamped owed finish.
No successor prompt-start or provider request is created. Once the successor
prompt has started, its real provider response remains the sole terminal owner
on the original branch.

## Harness notices

`harness.notice` carries a stable `kind`, a user-facing `message`, a
`NoticeLevel`, and `purpose = response | alert | diagnostic`. Purpose states why
the notice exists from the user's perspective and remains orthogonal to severity
and provenance. Treat `kind` values as protocol identifiers; do not derive them
from unstable connection ids or free-form message text, and do not infer purpose
from message prose. Critical notices remain defensively must-see.

## Session directory status

`harness.session_dir` is a UI/status snapshot, not proof that a durable session
directory exists. In session-ephemeral mode the harness reports
`SessionDirStatus::Ephemeral` and a display-only `<ephemeral>` path. Protocol
consumers must treat that as "no inspectable session directory"; they must not
try to derive persistent session storage from the sentinel path.

## Ephemeral agent markers

`ui.create_agent.ephemeral` requests a memory-only agent at the UI-to-harness
creation boundary. `agent.started.ephemeral` and
`session.agent_loaded.ephemeral` announce the resulting live state to UIs and
extensions. These markers describe Tau's local semantic stores only: protocol
consumers must not assume providers, tools, durable recipient agents, or
extensions forget data merely because an agent is ephemeral.
Harness-wide memory-only mode also sets both markers for every agent because
its transcript and session membership are process-local even when the creation
request did not independently request per-agent ephemerality.

## Validated identifiers

Wire identifiers such as `ToolName` and `ToolGroupName` are validated newtypes. Do not add default constructors that create values rejected by serde deserialization. Shared validation helpers should be kept in sync across equivalent identifier types.

`ModelTag` and `ToolTag` are also validated wire identifiers. They are metadata, not policy: providers/extensions publish tags, while the harness interprets them when assembling prompt tool surfaces.

## Format changes

Internal protocol changes follow
[GATE-no-backward-compatibility](../../../specs/GATE-no-backward-compatibility.md).
Required and optional fields express only the current protocol semantics.

## Agent metadata protocol

`agent.metadata_set_request` and `agent.metadata_unset_request` are
`persist=false`-by-default peer mutation requests. They reuse the canonical payload
shapes but never enter semantic replay for either Emit metadata value.
`agent.metadata_set` and `agent.metadata_unset` are separate harness-authored,
durable, extension-visible agent facts. Metadata keys are strings; values are
arbitrary CBOR values capped by `MAX_AGENT_METADATA_VALUE_BYTES`; and
`metadata_set.inheritable` controls child-agent copies. Replay uses the latest
folded canonical snapshot before `session.agent_loaded`. See
[SPEC-agent-metadata-requests-and-canonical-facts](../../../specs/SPEC-agent-metadata-requests-and-canonical-facts.md).

## Subscription replay protocol

`Subscribe` carries separate `historical_selectors` and `live_selectors`.
Historical catch-up is represented only by `EventDelivery.replay` on the
delivery envelope; event payloads are identical for catch-up and live
occurrences. Catch-up includes durable facts and harness-reconstructed current
snapshots selected by `historical_selectors`; both are delivered with
`replay: true`. Replay catch-up terminates with transient non-replay
`agent.replay_complete`/`session.replay_complete` boundary events before live
delivery is released.

## Prompt lifecycle versus provider prompt payloads

`agent.prompt_started` is the durable harness-authored materialization fact. It
carries prompt, agent, session, model, captured model parameters, owning outer
turn, originator, correlation, and operation metadata without provider content.
Agent-journal replay folds it for uniqueness
and inference-generation authority, but subscriber historical catch-up excludes
it.

`agent.outer_turn_started` and `agent.outer_turn_finished` are durable,
harness-authored activation boundaries. Their stable ids, session attribution,
initiating durable occurrence, and terminal disposition are accounting authority;
replay never infers a missing finish except when a validated output-length
continuation terminal with `outer_turn_finish_owed=true` or a terminal-owned
automatic-compaction decision explicitly authorizes one matching settled finish
repair.

A canonical `provider.response_finished`, or a harness-authored canceled
`agent.prompt_terminated` when no provider response exists, may carry one
`automatic_compaction_decision`. Its bounded transaction id, outer-turn id,
resolved model, and threshold are durable authority; rule names and work-status
state are not. A response's assistant node supplies the cut; a canceled
termination uses its durable parent because it appends no transcript node. The
matching outer-turn finish must reference the decision before a standalone start
with `automatic_policy` can claim the same identity. Replay repairs an owed finish
and start exactly once.

An output-length plan whose branch becomes dormant may append its exact reserved
steer, successor owner, pre-start failure, and owed finish under explicit parents
without changing the selected sibling. This repair is valid only before the
reserved successor's `agent.prompt_started`. After prompt-start, the dispatched
owner remains the sole terminal authority and branch movement cannot synthesize
a competing failure.

`agent.prompt_created` is the full transient provider work request and may carry
large system prompts, context, images, and tool definitions. It is emitted only
from the matching prompt-start fact's live post-commit continuation and never
enters semantic persistence. UI and observer lifecycle tracking should subscribe
to `agent.prompt_started` instead of the full provider payload.
This split is governed by
[SPEC-compact-prompt-materialization-authority](../../../specs/SPEC-compact-prompt-materialization-authority.md).

## Prompt-draft scope

`ui.prompt_draft` defaults to transient and is runtime-only rather than
transcript truth. Its optional `text` is contentful user input only when a
producer supplies it; content-free observations remain typing liveness.
Consumers that store, restore, synchronize, autocomplete, or otherwise
maintain state from supplied draft text must key that state by both `session_id`
and `target_agent_id`. A missing `target_agent_id` means an
unscoped/session-level draft, normally the start-new-agent prompt; consumers
must not infer the current agent from absence.
Only an attached socket UI may author draft observations. The event default is
transient, although the interactive CLI's established Emit wrapper currently
sends a true `persist` bit. The harness preserves that metadata but excludes
the event from semantic stores and replay for either value. `ui.focus_changed`
uses the same authority and persistence contract. See
[SPEC-ui-prompt-draft-and-focus-events](../../../specs/SPEC-ui-prompt-draft-and-focus-events.md).

## Shared agent navigation mode

Every `agent.stats_updated` complete operational snapshot carries a required
navigation mode independently of runtime state and the harness-owned canonical
work-status phase/title. The status is `unreported` without an accepted report;
reported phases require the validated canonical title, while `unknown` retains
an optional last valid title after the harness invalidates prior work. Transient
`ui.set_agent_navigation_mode` requests absolute changes; requester-directed
results acknowledge processing but do not replace the authoritative snapshot.
Successful admission of an authenticated visible human `ui.prompt_submitted` to
an existing loaded target is an implicit absolute `active` write. It produces no
navigation result, but the harness broadcasts a fresh complete stats snapshot
before queue or dispatch, including for an already-active target.

Explicit requests and accepted prompts share event-loop last-write-wins
ordering. Selection, rejected or non-visible prompts, later queue/steer
promotion, and replay do not write. Navigation state is daemon-lifetime state:
same-daemon catch-up reports the current value, while cold restore recomputes
defaults rather than deriving it from durable prompt history. UI caches change
only from complete stats snapshots, never outgoing or replayed prompt events.

## Directed current-session agent roster

`get_current_session` carries a correlation id and `current_session_result`
echoes it with the harness-owned current session id and absolute canonical
startup project root.

`get_session_agent_list` carries a correlation id, exact current session id, and
`current` or `history` scope. `session_agent_list_result` echoes the correlation
and session ids and carries either every row or one whole-request error with no
partial rows.

Row lifecycle is `live`, `unavailable`, or `unloaded`. Runtime and navigation
fields are present only for `live`. Persistence is `durable` or `ephemeral`.
Creation-fact status is `available`, `missing`, `invalid`, or `unreadable`;
available rows may carry start time, parent, creation role, and an in-memory or
checkpoint display-name projection. Live rows also carry the harness's current
runtime-only canonical work-status phase and title; rows without a live agent
carry no work-status snapshot. Phase and title invariants follow
[`SPEC-agent-watch`](../../../specs/SPEC-agent-watch.md).

The fixed limits are 4096 distinct agents checked against the maintained
membership cache before ids are cloned, 256 KiB for one first creation record,
4 MiB aggregate creation/checkpoint projection work, and the shared 16 MiB
encoded protocol message bound enforced by a limited writer. Stable
whole-request errors distinguish stale session, membership-projection
inconsistency, entry overflow, aggregate enrichment overflow, and encoded
response overflow.
