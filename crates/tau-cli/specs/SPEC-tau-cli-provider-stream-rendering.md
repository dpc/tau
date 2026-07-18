# SPEC-tau-cli-provider-stream-rendering: Provider stream rendering

Constrained by [SPEC-provider-response-streaming](../../../specs/SPEC-provider-response-streaming.md).

Terminal streaming accumulates `provider.response_updated.deltas` per prompt and provider output index. If a UI sees a delta for an unknown in-flight prompt, it may create a live block with an ellipsis prefix to indicate missed earlier transient deltas; the final `provider.response_finished` replaces live content with complete durable output. Provider status updates are rendered as transient status text and do not enter assistant response accumulation.

Live prompt output inside the terminal active area has a stable semantic order:
thinking, provider compaction status, assistant response, then active tool
summary/tool-call blocks. New live response-side blocks must be inserted before
active tool anchors rather than appended after them, so running tool UI remains
pinned nearest the prompt while assistant text continues streaming.
