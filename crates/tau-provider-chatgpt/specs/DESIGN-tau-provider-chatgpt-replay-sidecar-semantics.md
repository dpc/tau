# DESIGN-tau-provider-chatgpt-replay-sidecar-semantics: Responses replay sidecars are syntax, not semantics

Status: unconfirmed

Responses full-transcript replay preserves provider-visible syntax sidecars for
fields Tau does not semantically model, including raw tool-call argument JSON,
tool-call item envelopes, opaque reasoning/compaction items, and raw assistant
`message` items. These sidecars protect provider cache identity and replay
continuity when a turn cannot rely on `previous_response_id`.

Typed Tau fields remain authoritative. Tool routing uses parsed `ToolCallItem`
fields, assistant message replay rebases text and phase from `MessageItem`, and
raw assistant message sidecars are used only after validating that they are
Responses assistant `message` items.
