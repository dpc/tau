# ARCH-tau-proto: tau-proto architecture

`SessionId` and `AgentPromptId` are controlled wire and journal identifiers.
Both accept 1 through 128 bytes containing only ASCII letters, digits, `_`, and
`-`. Construction and deserialization validate that grammar; invalid protocol
or historical journal values fail closed rather than receiving a compatibility
or sanitization path.

The protocol now separates transient `message.*_reported` inputs from
harness-authored canonical `message.*` facts, carries declared
orthogonal `PeerCapability::MessageBridge` and
`PeerCapability::ActionProvider` authority in `Hello`, and separates transient
provider `provider.models_declared` inputs from harness-authored canonical
`provider.models_updated` current state. The generic authenticated publisher
delivery envelope and the remaining exact event-family mappings required by
[SPEC-peer-event-publication](../../../specs/SPEC-peer-event-publication.md)
remain to be implemented.

Configured extensions request user-visible diagnostics with the dedicated
`extension_notice_request` input message carrying only `message` and `level`.
The request is not an event and cannot set publication metadata; the harness
creates the resulting live-only `harness.notice`. See
[SPEC-extension-notice-requests](../../../specs/SPEC-extension-notice-requests.md).

Tool lifecycle uses distinct transient peer declarations
(`tool.registration_declared`, `tool.unregistration_declared`) and protected
harness-authored canonical state (`tool.register`, `tool.unregister`). Canonical
payloads identify the configured extension and its harness-assigned logical
instance; this is stable across supervised process respawn rather than a
process-connection generation. See
[SPEC-tool-declarations-and-canonical-state](../../../specs/SPEC-tool-declarations-and-canonical-state.md).
`extension.prompt_fragment_publish` remains the extension-authored declaration
name but now defaults to transient and never enters semantic history. See
[SPEC-prompt-fragment-declarations-and-projection](../../../specs/SPEC-prompt-fragment-declarations-and-projection.md).
Per-agent context registration, values, and readiness retain their existing wire
names, default to transient, and remain runtime-only observations. See
[SPEC-per-agent-context-declarations-and-readiness](../../../specs/SPEC-per-agent-context-declarations-and-readiness.md).
Tool progress likewise separates transient Tool/Core
`tool.progress_reported` observations from protected harness-authored canonical
`tool.progress` facts. Both share the `ToolProgress` payload, but the event names
distinguish a committed peer report from the canonical harness fact. The
enclosing `HarnessInputMessage::Emit` is the peer's private submission message;
see
[SPEC-tool-progress-reports-and-canonical-facts](../../../specs/SPEC-tool-progress-reports-and-canonical-facts.md).
Terminal tool outcomes likewise use transient peer
`tool.result_reported`, `tool.error_reported`, and
`tool.cancelled_reported` inputs, distinct from protected harness-authored
canonical terminal and provider projections. Reports and canonical facts reuse
the existing full terminal payload DTOs. Successful UI display events instead
use dedicated payload-free DTOs, so `tool.result_display` and
`tool.background_result_display` cannot expose raw result data. Event names
establish authorship and commit stage. See
[SPEC-terminal-tool-reports-and-canonical-outcomes](../../../specs/SPEC-terminal-tool-reports-and-canonical-outcomes.md).
User-shell progress/completion likewise separates transient Tool/Core
`shell.command_progress_reported` / `shell.command_finished_reported` inputs
from harness-authored canonical `shell.command_progress` /
`shell.command_finished` facts. Reports and facts reuse the existing shell
payload DTOs; `command_id` is the private provider route in reports and the
public UI lifecycle id in canonical facts. See
[SPEC-shell-command-reports-and-canonical-facts](../../../specs/SPEC-shell-command-reports-and-canonical-facts.md).
The unchanged `tool.request` name is a peer routing intent from configured
Provider/Tool/Core extensions. Its enclosing `Emit.persist` selects live-only
(`false`) or session-restore (`true`) publication; durable replay carries stable configured
publisher provenance but never requests execution. See
[SPEC-tool-requests-and-routing](../../../specs/SPEC-tool-requests-and-routing.md).
[SPEC-peer-event-publication](../../../specs/SPEC-peer-event-publication.md)
governs publication metadata and persistence classification.

Architectural or externally meaningful functional changes to protocol-facing
event persistence or the harness-extension interface require the explicit
confirmation mandated by
[GATE-persistence-and-extension-interface-change-approval](../../../specs/GATE-persistence-and-extension-interface-change-approval.md).

Detailed turn activity is a required transient field in complete
`agent.stats_updated` snapshots and an optional field in live roster entries.
Tool declarations classify active calls through the reserved
`tau:turn:manipulator`, `tau:turn:data_fetch`, and `tau:turn:wait` tags.
Configured Tool/Core extensions may replace their bounded, transient
per-agent `agent.runtime_indicators_declared` contribution; the only current
indicator is `timer_scheduled`.

Prompt lifecycle deliberately separates the durable, content-free
`agent.prompt_started` authority fact from the transient, content-bearing
`agent.prompt_created` provider request. Their ordering, replay, and compatibility
contract is governed by
[SPEC-compact-prompt-materialization-authority](../../../specs/SPEC-compact-prompt-materialization-authority.md).

Transient `extension.internal_prompt_submit_request` carries optional
`internal_prompt` or `timer` activation provenance. Absence preserves ordinary
internal-prompt behavior. The field adds no publisher authority; the harness
copies only the classification into its content-free queued-activation fact.
See
[SPEC-internal-prompt-submit-requests](../../../specs/SPEC-internal-prompt-submit-requests.md).

Bounded provider quota reports are transient provider observations.

Provider execution also separates five Provider-authored `_reported` observations from
four harness-canonical provider facts and the directed UI retry outcome. The old
unreported provider retry-result event no longer exists. See
[SPEC-provider-execution-reports-and-canonical-facts](../../../specs/SPEC-provider-execution-reports-and-canonical-facts.md).

Cache-refresh lifecycle adds directed sensitive harness requests plus a
content-free Provider terminal report/canonical fact. Its nonpersistence,
routing, deadline, and correlation contract is
[SPEC-provider-cache-refresh-lifecycle](../../../specs/SPEC-provider-cache-refresh-lifecycle.md).

`provider.quota_replace_reported`, `provider.quota_patch_reported`, and
`provider.quota_clear_reported` carry opaque profile epochs, strict sequences,
complete stable-key window records with independent usage/timing clocks, and
exact `ModelId` route bindings. Only the harness publishes protected
`harness.provider_quota_changed` full current snapshots after downstream
validation. None of these events are semantic transcript history. The trust and
pacing contract is
[SPEC-provider-quota-pacing](../../../specs/SPEC-provider-quota-pacing.md).

`tau-proto` owns Tau's shared wire data transfer objects and codec helpers. Treat every public type here as protocol surface unless it is explicitly private to tests.
`AgentId` wire decoding, serialization, durable parsing, equality, and display
use only the canonical unsigiled identifier. User-input parsers may call the
separate reference parser, which removes exactly one optional leading `@`
before applying the canonical grammar; it does not widen accepted wire or
stored values.

Agent lifecycle includes the content-free
`agent.user_interaction_recorded` durable fact. Its persisted-record timestamp
represents acceptance of a visible UI interaction without duplicating prompt
text. The summary projection contract is
[ARCH-tau-core](../../tau-core/specs/ARCH-tau-core.md).

Durable `agent.prompt_submitted` and `agent.prompt_steered` facts carry an
optional `InternalPromptKind`. `context_size_alert` marks the existing fact at
which that harness-owned alert reaches model context; it has specialized
presentation. `background_tool_completion` identifies the harness lifecycle
notice for a completed background tool so human UIs can suppress it without
classifying prompt prose. The contract is
[SPEC-compaction-and-context-recovery](../../../specs/SPEC-compaction-and-context-recovery.md).
Both prompt facts also carry required harness-stamped
`PromptSubmissionSource` provenance while retaining raw accepted prompt text.
They separately carry validated `trusted_internal_spans`; only those durable
ranges authorize `<tau_internal>` provider projection, while absent spans and
delimiter-shaped text remain ordinary payload. Core, harness, and CLI use the
source for routing and source-aware presentation: authenticated extension
prompts are attributed while typed HarnessInternal prompts use the diagnostic
setting. Protocol CBOR, durable authority, and UI consumers retain raw accepted
text. See
[SPEC-interactive-user-prompt-envelope](../../../specs/SPEC-interactive-user-prompt-envelope.md).

Durable tool terminals and transcript `ToolResultItem`s carry serde-defaulted
`ToolResultPresentation`. `tool_payload` is the extension-submitted default;
the harness alone stamps `harness_dedup_pointer`, including when retaining
terminal results through replay or compaction. `ToolCancelled` carries the same
presentation field and an optional complete-replacement `ToolUseState` display,
so cancellation can retain tool-owned generic presentation through canonical
publication and replay.

Protocol version 0 requires an extension's first harness response after
`Hello` to be `Configure`. Its optional validated `ToolNamePrefix` establishes
the connection's immutable structural name scope as specified by
[SPEC-extension-tool-prefixes](../../../specs/SPEC-extension-tool-prefixes.md).
The configured extension instance name is required and supplies the stable
harness-stamped publisher ID for canonical `message.*` facts derived
downstream from extension-published `message.*_reported` events.
Their wire and validation contract is
[SPEC-external-message-reports-and-facts](../../../specs/SPEC-external-message-reports-and-facts.md).
The same configured instance identity appears as `publisher_extension_id` in
harness-authored `provider.models_updated` snapshots derived downstream from
provider `provider.models_declared` events.
The protocol has no extension user-message prompt request; the only extension prompt
request is the narrow `extension.internal_prompt_submit_request` control path
with `agent_id`, `text`, optional `ctx_id`, and optional typed `activation_kind`
(`internal_prompt` or `timer`). It defaults to `persist=false`; its
commit-before-effects contract is
[SPEC-internal-prompt-submit-requests](../../../specs/SPEC-internal-prompt-submit-requests.md).
`agent.start_request` is another `persist=false`-by-default configured-extension
request. The raw event commits live but never enters semantic history; accepted
and terminal outcomes retain their existing shared/directed routing. See
[SPEC-start-agent-requests](../../../specs/SPEC-start-agent-requests.md).
UI agent creation similarly carries a caller correlation id and receives one
transient requester-directed admission result. Its distinct prompt correlation
binds later transient pre-materialization failures and the provider prompt chain;
create admission does not claim prompt execution success. See
[SPEC-ui-create-agent-admission](../../../specs/SPEC-ui-create-agent-admission.md).
The protocol deliberately provides no generic transport registration, ingress
acknowledgement, reply-route, deduplication, or send-completion schema.
Streaming readers reject a single encoded protocol message larger than 16 MiB
before higher-level connection or activation queues receive it.

Provider debug captures use the dedicated `provider_debug_capture` input
message, not `Emit`. The message carries typed session/prompt attribution,
capture class, and opaque zstd bytes. It is non-journaled, debug-redacted, and
accepted only from an authenticated configured Provider; the harness owns path
selection and never parses or decompresses the payload.

Provider-visible images are transport-neutral binary values attached to tool
results, separate from message authorship. CBOR is the durable/IPC byte
transport; provider-specific data URLs are never protocol truth. Debug
representations summarize byte length rather than formatting image bytes.

## Directional messages and CBOR

Harness input and output messages are directionally typed. Keep request/response
envelopes in the correct enum. The internal protocol compatibility policy is
[GATE-no-backward-compatibility](../../../specs/GATE-no-backward-compatibility.md).

Message facts keep stable publisher-domain party and conversation identities
separate from optional mutable display presentation. Provider presentation
uses the opaque references and optional typed metadata specified by
[SPEC-external-message-reports-and-facts](../../../specs/SPEC-external-message-reports-and-facts.md),
and carries the harness-stamped publisher separately.
Shared visible escaping covers controls, bidi/zero-width/default-ignorable
structure, variation selectors, Hangul fillers, and noncharacters so UIs and
provider XML never interpret hostile metadata.

`encode_message` writes one self-delimiting CBOR item. `decode_message_from_slice` and the harness input/output slice helpers must decode exactly one item and reject trailing bytes; use `MessageReader` for streams of concatenated messages.

Extension data includes the configured-instance `Secret` scope and complete-file
generation compare-and-swap governed by
[SPEC-extension-secret-storage](../../../specs/SPEC-extension-secret-storage.md).
`ExtensionDataRequest.expected_session_id` optionally carries the request-level
Session-scope target; absence preserves older wire producers and lets the harness
substitute frame-admission identity. The typed `SessionMismatch` result reports a
target that is no longer current.
Secret request and result debug projections expose lengths and operation metadata,
never payload bytes.

External agent-message delivery is modeled as a dedicated directional RPC
(`external_agent_message` / `external_agent_message_result`) rather than as a
generic `emit`. The payload carries sender and recipient session ids separately
from slash-free `AgentId`s; do not encode `session/agent` into `AgentId`. Sender
authentication is a second dedicated RPC (`external_agent_message_auth`) that
validates a per-message capability before the recipient harness trusts the
caller-supplied sender identity, message/watch-response kind, or message body.
The authenticated recipient is a tagged `bare_entrypoint` or exact agent value,
so route authority cannot be substituted during callback authentication. A
successful response carries the concrete resolved agent id and started flag.

`peer_session_probe` is a separate narrow RPC that returns only whether the live
harness is currently bound to a claimed session and accepts bare inter-session
messages. It does not enumerate agents or expose receiving policy.

`get_current_session` is a local-control, requester-directed RPC that returns the
harness's in-memory current session id and immutable canonical startup project
root. Runtime discovery uses both fields to establish live identity without
trusting adjacent metadata. See
[SPEC-tau-proto-session-events](SPEC-tau-proto-session-events.md).

An interactive UI may bind `Hello.expected_session_id` to its selected target.
The harness admits the connection only when that value equals its current
session and replies with `ui_session_accepted` before accepting later UI
traffic. A mismatch receives an actionable disconnect instead, preventing a
stale or replaced runtime socket from silently attaching the UI elsewhere.

`get_session_agent_list` is a separate UI-only, requester-directed RPC for the
harness's exact current session. Its bounded result contains membership
lifecycle, persistence, runtime/navigation classification for live rows, and
content-minimized creation labels. Live rows also carry their current canonical
work-status phase/title snapshot. It is not an event and has no
extension/external request path.

Session discovery uses transient session-provider registration, complete atomic
session/agent source snapshots, and correlated readiness. `session.agent_loaded`,
per-agent snapshots, keyed context, readiness, the durable initialization
replacement, and the canonical agent projection carry a mandatory
`AgentInitializationId`. The harness alone authors the durable replacement and
canonical projections. Their contract is
[SPEC-session-discovery-declarations-and-readiness](../../../specs/SPEC-session-discovery-declarations-and-readiness.md).
Per-agent provider registration, keyed context publication, and readiness remain
transient and use the same exact initialization correlation. Their contract is
[SPEC-per-agent-context-declarations-and-readiness](../../../specs/SPEC-per-agent-context-declarations-and-readiness.md).

`term.osc1337_set_user_var` and `term.bell` retain their existing DTOs, wire
names, and caller-selected Emit metadata. The harness excludes both from semantic
stores, and terminal consumers act only on live delivery. See
[SPEC-terminal-output-side-effect-events](../../../specs/SPEC-terminal-output-side-effect-events.md).

`extension.event` carries an extension-owned nested event name, optional session
metadata, and opaque CBOR. Reserved first-party categories remain structurally
unrepresentable. Custom events are live subscriber traffic rather than semantic
history; authenticated publisher identity is not yet present on wire delivery.
See [SPEC-custom-extension-events](../../../specs/SPEC-custom-extension-events.md).

`ui.prompt_draft` and `ui.focus_changed` retain their typed payloads and transient
event defaults. Caller-selected Emit metadata remains separate: the interactive
CLI currently sends both with `persist=true`, while the harness excludes both
from semantic stores for either value. See
[SPEC-ui-prompt-draft-and-focus-events](../../../specs/SPEC-ui-prompt-draft-and-focus-events.md).

`agent.metadata_set_request` and `agent.metadata_unset_request` reuse the
canonical metadata payload shapes but are distinct `persist=false`-by-default peer
requests. Only harness-authored `agent.metadata_set` and
`agent.metadata_unset` are durable facts. See
[SPEC-agent-metadata-requests-and-canonical-facts](../../../specs/SPEC-agent-metadata-requests-and-canonical-facts.md).

`ui_debug_event_stats_request` is a flat peer-to-harness message rather than a
bus event. Its payload selects one configured extension by name; authorization
and the directed notice response remain harness concerns.
`ui_detach_request` is likewise a flat payload-free peer-to-harness message; its
authorized effect is local connection control rather than event publication.
`ui_tree_request` is a flat message carrying the session and optional target
agent; the harness returns its rendered tree only to the requesting UI as one
multiline notice. `ui.navigate_tree` remains a distinct state-changing event.

`agent.stats_updated` remains a transient complete runtime snapshot. Its
`estimated_api_cost` is self-only; its defaulted
`creator_subtree_estimated_api_cost` is the inclusive current-harness
authenticated-agent-creator total. Both reset with runtime/session state and
never become journal facts; new decoders synthesize zero when older payloads
omit the defaulted subtree field.
