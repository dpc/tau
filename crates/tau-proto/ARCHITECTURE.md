# tau-proto architecture

`tau-proto` owns Tau's shared wire data transfer objects and codec helpers. Treat every public type here as protocol surface unless it is explicitly private to tests.

## Directional messages and CBOR

Harness input and output messages are directionally typed. Keep request/response envelopes in the correct enum, and preserve existing serde names unless a migration plan updates all producers, consumers, docs, and recorded fixtures.

`encode_message` writes one self-delimiting CBOR item. `decode_message_from_slice` and the harness input/output slice helpers must decode exactly one item and reject trailing bytes; use `MessageReader` for streams of concatenated messages.

External agent-message delivery is modeled as a dedicated directional RPC
(`external_agent_message` / `external_agent_message_result`) rather than as a
generic `emit`. The payload carries sender and recipient session ids separately
from slash-free `AgentId`s; do not encode `session/agent` into `AgentId`.

## Provider-visible tool responses

Tool result events carry raw CBOR for non-provider consumers, but provider prompt construction must render tool outputs through `ToolResponse::render()`. That render path is the central defense-in-depth normalization boundary after tool-local semantic escaping.

`ToolResponse::render()` must keep headers as safe single lines, preserve ASCII LF body separators for line-oriented records, escape other model-visible control and separator characters, and keep binary/fallback rendering bounded. This is not terminal/UI escaping; terminal renderers still need their own sanitization for display state and layout.

Bounded model-visible diagnostic text helpers that are shared by harness, core,
and extensions may also live in `tau-proto` when they have no dependency on
harness state or extension implementation details. These helpers are part of the
same prompt-surface safety boundary as rendered tool responses: keep work
bounded, outputs deterministic, and tests close to the exported helper.

## Event names and routing

`Event` serde `rename` values, `EventName` constants, and `Event::name()` are one contract. When adding or renaming an event, update all three together and update `docs/events.md` when the selected guide should mention the event.

First-party event categories (`tool`, `action`, `agent`, `extension`, `provider`, `harness`, `ui`, `shell`, `session`, and `term`) are reserved for typed protocol events. `CustomEvent` names must use extension-owned categories so extension payloads cannot spoof first-party routing or policy keys.
Parsed event names and custom event payload names must have non-empty category and call segments; empty segments are malformed protocol data rather than extension-owned names.

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

## Provider response streaming updates

`provider.response_updated` is transient append-delta protocol surface for visible assistant/reasoning progress. Providers must send newly appended text in `deltas`, not full accumulated message snapshots; retry/status diagnostics belong in the separate `status` field because they are provider-authored, not assistant-authored. `provider.response_finished.output_items` remains the complete durable response and replay source.
