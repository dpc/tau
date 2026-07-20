# ARCH-tau-proto: tau-proto architecture

## Status

The current protocol lets extensions directly emit the six canonical
`message.*` facts and does not carry authenticated publisher metadata in the
generic event delivery envelope.
[DECISION-generic-peer-event-emission](../../../specs/DECISION-generic-peer-event-emission.md)
requires distinct bridge report events, harness-authored canonical facts, and
an immutable publisher envelope across interception, live delivery, persistence,
and replay. The current wire contract remains in force until that migration
completes.

Architectural or externally meaningful functional changes to protocol-facing
event persistence or the harness-extension interface require the separately
reviewed, human-confirmed decision mandated by
[DECISION-persistence-and-extension-interface-change-approval](../../../specs/DECISION-persistence-and-extension-interface-change-approval.md).

Bounded provider quota records are transient current state. Provider
replace/patch/clear events carry opaque profile epochs, strict sequences,
complete stable-key window records with independent usage/timing clocks, and
exact `ModelId` route bindings. Harness projections are full current snapshots;
none of these events are semantic transcript history. The trust and pacing
contract is [SPEC-provider-quota-pacing](../../../specs/SPEC-provider-quota-pacing.md).

`tau-proto` owns Tau's shared wire data transfer objects and codec helpers. Treat every public type here as protocol surface unless it is explicitly private to tests.

Agent lifecycle includes the content-free
`agent.user_interaction_recorded` durable fact. Its persisted-record timestamp
represents acceptance of a visible UI interaction without duplicating prompt
text. The summary projection contract is
[DECISION-tau-core-agent-summary-checkpoints](../../tau-core/specs/DECISION-tau-core-agent-summary-checkpoints.md).

Protocol version 0 requires an extension's first harness response after
`Hello` to be `Configure`. Its optional validated `ToolNamePrefix` establishes
the connection's immutable structural name scope as specified by
[DECISION-extension-tool-prefixes](../../../specs/DECISION-extension-tool-prefixes.md).
The configured extension instance name is required and supplies the stable
harness-stamped publisher ID for extension-published `message.*` facts.
Their wire and validation contract is
[SPEC-extension-published-message-facts](../../../specs/SPEC-extension-published-message-facts.md).
The protocol has no extension user-message prompt request; the only extension prompt
request is the narrow `extension.internal_prompt_submit_request` control path
with `agent_id`, `text`, and optional `ctx_id`.
The protocol deliberately provides no generic transport registration, ingress
acknowledgement, reply-route, deduplication, or send-completion schema.
Streaming readers reject a single encoded protocol message larger than 16 MiB
before higher-level connection or activation queues receive it.

Provider-visible images are transport-neutral binary values attached to tool
results, separate from message authorship. CBOR is the durable/IPC byte
transport; provider-specific data URLs are never protocol truth. Debug
representations summarize byte length rather than formatting image bytes.

## Directional messages and CBOR

Harness input and output messages are directionally typed. Keep request/response
envelopes in the correct enum. The internal protocol compatibility policy is
[DECISION-no-backward-compatibility](../../../specs/DECISION-no-backward-compatibility.md).

Message facts keep stable publisher-domain party and conversation identities
separate from optional mutable display presentation. Provider presentation
uses the opaque references and optional typed metadata specified by
[DECISION-common-external-message-envelope](../../../specs/DECISION-common-external-message-envelope.md),
and carries the harness-stamped publisher separately.
Shared visible escaping covers controls, bidi/zero-width/default-ignorable
structure, variation selectors, Hangul fillers, and noncharacters so UIs and
provider XML never interpret hostile metadata.

`encode_message` writes one self-delimiting CBOR item. `decode_message_from_slice` and the harness input/output slice helpers must decode exactly one item and reject trailing bytes; use `MessageReader` for streams of concatenated messages.

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
harness's in-memory current session id. Runtime discovery uses it to establish
lifecycle authority without trusting adjacent metadata.
See
[DECISION-current-session-control-rpc](../../../specs/DECISION-current-session-control-rpc.md).

`get_session_agent_list` is a separate UI-only, requester-directed RPC for the
harness's exact current session. Its bounded result contains membership
lifecycle, persistence, runtime/navigation classification for live rows, and
content-minimized creation labels. It is not an event and has no
extension/external request path.
