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

## Harness notices

`harness.notice` carries a stable `kind`, a user-facing `message`, a `NoticeLevel`, and optional `always_show`. Treat `kind` values as protocol identifiers: UIs may special-case them, so do not derive them from unstable connection ids or free-form message text. `critical` notices and `always_show` warnings represent mandatory diagnostics; the harness must keep emitting them even if a UI filters routine notices locally.

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
replay never infers a missing finish.

`agent.prompt_created` is the full transient provider work request and may carry
large system prompts, context, images, and tool definitions. It is emitted only
from the matching prompt-start fact's live post-commit continuation and never
enters semantic persistence. UI and observer lifecycle tracking should subscribe
to `agent.prompt_started` instead of the full provider payload.
This split is governed by
[SPEC-compact-prompt-materialization-authority](../../../specs/SPEC-compact-prompt-materialization-authority.md).

## Agent watch turn-state wire boundary

`agent.message_received` uses `kind = watch_turn_state` for receiver-only,
harness-authored outer agent-turn observations. The agent turn spans activating
input through terminal response or termination, while each provider invocation
is an inner model round and tool execution between invocations is a tool round.
Such records must carry
`watch_turn_state`; all other message kinds must omit it. The payload identifies
the session-local subscription, distinguishes an initial snapshot from an edge,
and carries the harness-runtime-scoped watched-agent turn generation.

## Prompt-draft scope

`ui.prompt_draft` defaults to transient and is runtime-only rather than
transcript truth, but it is still
contentful user input. Consumers that store, restore, synchronize, autocomplete,
or otherwise maintain state from prompt drafts must key that state by both
`session_id` and `target_agent_id`. A missing `target_agent_id` means an
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
navigation mode independently of runtime state. Transient
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
checkpoint display-name projection.

The fixed limits are 4096 distinct agents checked against the maintained
membership cache before ids are cloned, 256 KiB for one first creation record,
4 MiB aggregate creation/checkpoint projection work, and the shared 16 MiB
encoded protocol message bound enforced by a limited writer. Stable
whole-request errors distinguish stale session, membership-projection
inconsistency, entry overflow, aggregate enrichment overflow, and encoded
response overflow.
