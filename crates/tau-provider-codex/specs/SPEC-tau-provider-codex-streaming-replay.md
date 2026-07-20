# SPEC-tau-provider-codex-streaming-replay: Streaming and replay

This record refines
[SPEC-provider-response-streaming](../../../specs/SPEC-provider-response-streaming.md)
and
[SPEC-tau-proto-provider-data](../../tau-proto/specs/SPEC-tau-proto-provider-data.md).

## Streaming provider output

Responses streams may deliver visible assistant text, reasoning summaries, large
function-call arguments, or custom-tool input during an agent turn. Providers emit
displayable assistant/reasoning append deltas and final tool-call items, and publish
public content-free response throughput samples on
`provider.response_updated_reported.response_stats`.

The sampler starts when the backend request is dispatched. Lower-layer WebSocket
frame receives immediately advance the prompt-local received byte counter before
semantic parsing, while parsed chunks update pending visible/non-visible deltas.
The provider writes the first non-empty `provider.response_updated_reported` sample as soon as
streamed output is observed, then writes later non-terminal samples only on one-second
response deadlines; later byte changes never bypass that cadence. Each public
`response_stats` pair uses `previous` = the last provider sample actually emitted for
the prompt and `current` = the new cumulative sample. A terminal flush is the other
normal bypass and is allowed immediately before the provider prompt closes. The harness
validates provider ownership and broadcasts these stats unchanged; UI clients render
them directly from provider updates.

## Replay-sidecar trust boundary

Raw provider replay sidecars are external-provider-authored transcript data. They may
contain provider ids, status fields, annotations, encrypted reasoning blobs, or future
fields that Tau does not understand. Treat them as sensitive provider-visible syntax for
replay/cache fidelity, not as semantic authority.

Replay code must validate the sidecar item kind before reusing raw JSON and must rebase
controlled semantic fields from typed Tau structures. In particular, assistant `message`
sidecars may only replay as assistant messages, and their model-visible text/phase must
come from `MessageItem` rather than from an unchecked raw blob.

## Replay fidelity

Replay contributes to the same provider-visible cache identity. When replaying assistant
function calls, request construction must prefer `ToolCallItem.raw_arguments_json` so
object key order, whitespace, and numeric spelling match the provider's original
argument string. Serializing parsed CBOR arguments is only a fallback for older
persisted records that do not have the raw sidecar.

The same replay rule applies to opaque Responses provider items. Reasoning, compaction,
and unknown output items should be stored with `OpaqueProviderItem.raw_json` when the
upstream event JSON is available, and full transcript replay should prefer that sidecar
over the parsed CBOR `OpaqueProviderItem.value`.

Responses assistant `message` items also carry a replay sidecar. Tau keeps the typed
message text and `phase` as semantic truth, but the raw Responses item preserves
provider-owned ids, status, annotations, content-part boundaries, and unknown fields
that may affect server-side replay/cache behavior. Full transcript replay should emit
the raw item unchanged when its text and phase already match the typed fields and the
raw item validates as a Responses assistant `message`; otherwise it may parse the raw
item and update only text/phase before sending it, or synthesize from typed fields when
validation fails.

Responses tool-call output items split semantic tool-call routing from provider envelope
fidelity. `ToolCallItem.call_id`, name, type, and arguments remain the validated Tau
fields used for dispatch and tool-result pairing, while
`ToolCallItem.responses_envelope` stores the provider item id/status and unknown
non-structured fields needed to replay `function_call` and `custom_tool_call` items
without changing provider-visible item identity. The sidecar's `extra_fields` is a
parsed CBOR map of JSON object members; it preserves values, not raw JSON
spelling/order, and replay ignores non-map values. Extra fields cannot override rebuilt
structured fields such as `id`, `status`, `call_id`, `name`, `arguments`, or `input`.
Full transcript replay must fall back to the historical `fc_`/`ctc_` id synthesis when
that sidecar is absent.
