# tau-proto architecture

`tau-proto` owns Tau's shared wire data transfer objects and codec helpers. Treat every public type here as protocol surface unless it is explicitly private to tests.

## Directional messages and CBOR

Harness input and output messages are directionally typed. Keep request/response envelopes in the correct enum, and preserve existing serde names unless a migration plan updates all producers, consumers, docs, and recorded fixtures.

`encode_message` writes one self-delimiting CBOR item. `decode_message_from_slice` and the harness input/output slice helpers must decode exactly one item and reject trailing bytes; use `MessageReader` for streams of concatenated messages.

External agent-message delivery is modeled as a dedicated directional RPC
(`external_agent_message` / `external_agent_message_result`) rather than as a
generic `emit`. The payload carries sender and recipient session ids separately
from slash-free `AgentId`s; do not encode `session/agent` into `AgentId`. Sender
authentication is a second dedicated RPC (`external_agent_message_auth`) that
validates a per-message capability before the recipient harness trusts the
caller-supplied sender identity, message/watch-response kind, or message body.

## Provider-visible tool responses

Tool result events carry raw CBOR for non-provider consumers, but provider prompt construction must render tool outputs through `ToolResponse::render()`. That render path is the central defense-in-depth normalization boundary after tool-local semantic escaping.

`ToolResponse::render()` must keep headers as safe single lines, preserve ASCII LF body separators for line-oriented records, escape other model-visible control and separator characters, and keep binary/fallback rendering bounded. This is not terminal/UI escaping; terminal renderers still need their own sanitization for display state and layout.

Bounded model-visible diagnostic text helpers that are shared by harness, core,
and extensions may also live in `tau-proto` when they have no dependency on
harness state or extension implementation details. These helpers are part of the
same prompt-surface safety boundary as rendered tool responses: keep work
bounded, outputs deterministic, and tests close to the exported helper.

## Tool-call argument dual representation

`ToolCallItem.arguments` is the parsed CBOR semantic form of assistant tool-call
arguments. Harness validation, tool routing, repair, and dispatch must use this
field as the source of truth.

`ToolCallItem.raw_arguments_json` is an optional replay/cache-identity sidecar
for provider function calls whose upstream wire format supplied arguments as a
JSON string. Providers should populate it with the exact original string while
also parsing into `arguments`; prompt replay should prefer the sidecar when
reconstructing provider history and fall back to serializing `arguments` only for
old persisted records or calls without provider-wire JSON. The sidecar is not a
semantic authority and must not bypass CBOR validation/dispatch.

`ToolCallItem.responses_envelope` is an optional Responses-only replay sidecar
for provider-owned tool-call output item envelope fields such as item `id`,
`status`, and unknown future fields. It must not contain semantic fields such as
`call_id`, `name`, `arguments`, or `input`; those are rebuilt from the validated
`ToolCallItem` fields so harness-side id normalization and dispatch remain
authoritative. Responses replay should prefer the provider item id from this
sidecar and fall back to deterministic `fc_`/`ctc_` synthesis only for old
records.

## Opaque provider item dual representation

`OpaqueProviderItem.value` is the parsed CBOR form of provider-owned output
items such as Responses reasoning, compaction, and unknown future provider items.

Standalone compaction control uses bounded transaction identifiers and
harness-owned started, failed, and inference-dispatch checkpoint facts. New
starts pre-mint and persist the compact prompt/model/standalone-operation tuple;
successful boundaries repeat that tuple so replay can validate exact provider
work ownership. Canonical submitted, injected, and steered inputs carry a
default-false `inference_activation` marker: new activating writes set it true,
while passive and legacy facts cannot independently replay-wake inference.
`agent.compacted` records carry immutable cut and suffix metadata; metadata-free
historical records retain legacy hard-boundary replay semantics.
It exists for semantic inspection and compatibility with protocol consumers that
need structured data.

`OpaqueProviderItem.raw_json` is an optional provider-visible replay sidecar for
the exact JSON item emitted by a backend. Responses request reconstruction should
prefer this sidecar so key order and numeric spelling remain stable for upstream
cache identity, and fall back to serializing `value` only for older records or
items that were not captured from provider JSON.

## Responses assistant message dual representation

`MessageItem.role`, `MessageItem.content`, and `MessageItem.phase` are Tau's
semantic assistant-message truth. Rendering, prompt display, and any future
message-level validation must use those typed fields.

`MessageItem.responses_raw_json` is an optional Responses-only replay sidecar for
assistant `message` output items. Responses request reconstruction may reuse it
to preserve provider item ids, statuses, annotations, content-part boundaries,
and unknown fields that are not semantically modeled by Tau. Replay must still
rebase the provider-visible text and `phase` from the typed fields, and must
drop the sidecar for non-Responses providers. The sidecar is replay-eligible
only when it decodes as a Responses assistant `message`; otherwise providers
must synthesize replay from the typed fields.

## Provider-visible message envelopes

All `MessageOperation` variants share one compact, valid `<tau_message>`
provider projection. Harness-authored routing facts use canonically ordered XML
attributes and text-bearing operations use the element's direct escaped text;
delete and reaction occurrences are self-closing. `origin="external"` means
all payload text is untrusted. `sender_allowlisted="true"` means the operator
allowlisted the authenticated sender, while `"false"` means lax policy admitted
an outsider; neither value grants instruction, tool, or control authority.

Attribute and text escaping are deliberately separate. Attribute controls,
line separators, bidi/format controls, and noncharacters become visible
`\u{XXXX}` escapes; element text preserves ordinary LF/tab but visibly escapes
unsafe controls and format characters. Reply attributes are transient: prompt
assembly includes one only while the source-bound route is live and its named
tool is present in that agent's effective tool policy.

## Event names and routing

`Event` serde `rename` values, `EventName` constants, and `Event::name()` are one contract. When adding or renaming an event, update all three together and update `docs/events.md` when the selected guide should mention the event.

First-party event categories (`tool`, `action`, `agent`, `extension`, `provider`, `harness`, `ui`, `shell`, `session`, and `term`) are reserved for typed protocol events. `CustomEvent` names must use extension-owned categories so extension payloads cannot spoof first-party routing or policy keys.
Parsed event names and custom event payload names must have non-empty category and call segments; empty segments are malformed protocol data rather than extension-owned names.

`provider.response_updated.response_stats` is public provider-owned response-liveness metadata. It is content-free, prompt-local, and owned by the provider because the provider owns the backend request lifecycle and reads the response byte stream. Providers attach previous/current cumulative samples to rate-limited response updates: `previous` is the last sample that was actually emitted for that provider prompt, and `current` is the new cumulative sample. Providers may emit the first non-empty response/progress/stat update immediately so UIs learn that output has started. Later non-terminal provider response/progress/stat updates must not be emitted more than once per second per prompt; later byte changes never bypass that cadence. A final flush may bypass the cadence immediately before the provider prompt closes.

The harness validates provider prompt ownership and routing for `provider.response_updated`, but it must not consume, strip, remap, account, or project provider response stats. UI clients render response throughput directly from the provider update. Stats-only provider updates are valid public transient events.

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

## Compatibility expectations

Prefer additive optional fields with serde defaults for backward compatibility. Required fields should be intentional and covered by tests when missing data would make downstream UI, harness, or provider behavior ambiguous.

## Agent metadata protocol

`agent.metadata_set` and `agent.metadata_unset` are durable, extension-visible agent facts. Metadata keys are strings; values are arbitrary CBOR values capped by `MAX_AGENT_METADATA_VALUE_BYTES`; and `metadata_set.inheritable` controls child-agent copies. Do not classify these events as transient defaults: extensions may subscribe to them for live state, and replay uses the latest folded snapshot before `session.agent_loaded`.

## Subscription replay protocol

`Subscribe` carries separate `historical_selectors` and `live_selectors`.
Historical catch-up is represented only by `EventDelivery.replay` on the
delivery envelope; event payloads are identical for catch-up and live
occurrences. Catch-up includes durable facts and harness-reconstructed current
snapshots selected by `historical_selectors`; both are delivered with
`replay: true`. Replay catch-up terminates with transient non-replay
`agent.replay_complete`/`session.replay_complete` boundary events before live
delivery is released.

## Provider response streaming updates

`provider.response_updated` is transient append-delta protocol surface for
visible assistant/reasoning progress. Providers must send newly appended text in
`deltas`, not full accumulated message snapshots; retry/status diagnostics belong
in the separate `status` field because they are provider-authored, not
assistant-authored. Live byte/duration stats are provider-owned content-free metadata carried in
`response_stats`; UIs render them directly from `provider.response_updated`.
`provider.response_finished.output_items` remains the complete durable response
and replay source.

## Prompt lifecycle versus provider prompt payloads

`agent.prompt_created` is the full provider work request and may carry large
system prompts, context, and tool definitions. UI and observer lifecycle
tracking should use the transient `agent.prompt_started` companion instead of
subscribing to the full provider payload.

## Agent watch turn-state wire boundary

`agent.message_received` uses `kind = watch_turn_state` for receiver-only,
harness-authored outer agent-turn observations. The agent turn spans activating
input through terminal response or termination, while each provider invocation
is an inner model round and tool execution between invocations is a tool round.
Such records must carry
`watch_turn_state`; all other message kinds must omit it. The payload identifies
the session-local subscription, distinguishes an initial snapshot from an edge,
and carries the harness-runtime-scoped watched-agent turn generation.
## Provider tool-type metadata

`ProviderModelInfo.supported_tool_types` is provider-published capability
metadata. Omitted or empty metadata is legacy-compatible Function-only support;
Custom tools require explicit publication. The harness may narrow this set with
policy but must not widen it. Changes require coordinated provider, harness,
wire-compatibility, and serialization tests.

`ProviderResponseFinished.failure_kind` carries a closed, display-prose-independent category for
terminal provider request rejection. `context_window_exceeded` allows lifecycle consumers to
reason about the outcome without parsing the bounded human-readable `error` field.

## Structured watched provider status

Transient retry updates may carry `ProviderRetryStatus`: a closed work category, saturating attempt number, and approximate whole-second delay. Human status text remains UI presentation and is not an authority for harness decisions. `AgentWatchProviderStatusNotification` is the harness-authored cross-agent projection and contains only bounded facts, prompt/turn correlation, watch subscription identity, and a nested serde-tagged `state`. The `phase` discriminator selects `retrying`, `recovering_context`, `blocked`, `dispatch_uncertain`, or `terminal_error`; each variant owns exactly the category, attempt, delay, or failure fields valid for that phase, so contradictory option combinations are neither constructible nor decodable. `recovering_context` is reserved for the separately approved reactive-compaction implementation; retry, terminal, blocked-compaction, and restored dispatch-uncertain projections are emitted today.

### Reactive context recovery correlation

Context-overflow recovery is correlated entirely by durable facts. Inference checkpoints capture the provider-qualified model, operation, and immutable pre-activation cut. The harness, never the provider, stamps an eligible terminal response as `reactive_compaction_planned`; a reactive standalone-compaction start then uniquely claims that failed prompt id. Legacy checkpoints omit the new cut facts and are recovery-ineligible.
### Durable manual-compaction facts

`agent.manual_compaction_requested` records harness-owned pre-start
correlation. Exactly one matching `agent.standalone_compaction_started` with a
`manual_agent_tool` trigger or
`agent.manual_compaction_request_failed` may terminate that pre-start state.
These control facts are persisted for durable agents and use the same
memory-only semantics as other facts for ephemeral agents.
Terminal context-window responses may carry optional harness-authored
`context_limit_telemetry`. The harness overwrites extension input and correlates
the immutable dispatch snapshot by prompt, exact model, and operation. Closed
observation, policy, eligibility, and action tags contain no prompt/provider
body. `action=reactive_compaction_planned` is valid only with the matching
`recovery_disposition`; absent fields preserve legacy decoding.
