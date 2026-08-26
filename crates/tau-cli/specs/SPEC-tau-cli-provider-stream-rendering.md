# SPEC-tau-cli-provider-stream-rendering: Provider stream rendering

Constrained by [SPEC-provider-response-streaming](../../../specs/SPEC-provider-response-streaming.md).

Terminal streaming accumulates `provider.response_updated.deltas` per prompt and provider output index. If a UI sees a delta for an unknown in-flight prompt, it may create a live block with an ellipsis prefix to indicate missed earlier transient deltas; the final `provider.response_finished` replaces live content with complete durable output. Provider status updates are rendered as transient status text and do not enter assistant response accumulation.

At renderer dequeue, the CLI may fold a contiguous, already-admitted run of
ordinary `provider.response_updated` facts for the same agent, prompt, and
originator. It concatenates deltas in delivery/vector order and spans response
stats from the first sample's `previous` to the last sample's `current`, while
retaining the first observed first-semantic duration. Status, compaction,
lifecycle, disconnect, other-event/prompt, and captured local-command
watermarks are barriers. The renderer probes only currently queued work and
dispatches immediately; it never waits to enlarge a run. Consequently,
intermediate states within one folded run need not draw, while canonical FIFO,
queue accounting, and per-frame delivery diagnostics remain exact.

`agent.prompt_started` with `operation = standalone_compaction` identifies a
private compactor stream. The CLI renders a compact content-free progress marker
for that prompt and suppresses its provider reasoning and message updates,
assistant/editor history, and replacement checkpoint. Its matching compacted,
failed, or terminated lifecycle outcome replaces or removes the marker and
forgets the prompt correlation. This local presentation state does not alter
provider events, journals, replay authority, or compaction replacement facts.

Live prompt output inside the terminal active area has a stable semantic order:
thinking, provider compaction status, assistant response, then active tool
summary/tool-call blocks. New live response-side blocks must be inserted before
active tool anchors rather than appended after them, so running tool UI remains
pinned nearest the prompt while assistant text continues streaming.
