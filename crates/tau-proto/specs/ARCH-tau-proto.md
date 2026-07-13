# ARCH-tau-proto: tau-proto architecture

Bounded provider quota records are transient current state. Provider
replace/patch/clear events carry opaque profile epochs, strict sequences,
complete stable-key window records with independent usage/timing clocks, and
exact `ModelId` route bindings. Harness projections are full current snapshots;
none of these events are semantic transcript history. The trust and pacing
contract is [DESIGN-provider-quota-pacing](../../../specs/DESIGN-provider-quota-pacing.md).

`tau-proto` owns Tau's shared wire data transfer objects and codec helpers. Treat every public type here as protocol surface unless it is explicitly private to tests.

Provider-visible images are transport-neutral binary values attached to tool
results, separate from message authorship. CBOR is the durable/IPC byte
transport; provider-specific data URLs are never protocol truth. Debug
representations summarize byte length rather than formatting image bytes.

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
The authenticated recipient is a tagged `bare_entrypoint` or exact agent value,
so route authority cannot be substituted during callback authentication. A
successful response carries the concrete resolved agent id and started flag.

`peer_session_probe` is a separate narrow RPC that returns only whether the live
harness is currently bound to a claimed session and advertises an effective
peer entrypoint. It does not enumerate agents or expose entrypoint policy.
