# SPEC-tau-cli-provider-stream-rendering: Provider stream rendering

## Record justification

This record owns the non-local streaming-to-settled frame contract shared by provider event projection, transcript state, editor context, and terminal redraw scheduling.

Constrained by [SPEC-provider-response-streaming](../../../specs/SPEC-provider-response-streaming.md).

Terminal streaming accumulates `provider.response_updated.deltas` per prompt and provider output index. If a UI sees a delta for an unknown in-flight prompt, it may create a live block with an ellipsis prefix to indicate missed earlier transient deltas; the final `provider.response_finished` replaces live content with complete durable output. Provider status updates are rendered as transient status text and do not enter assistant response accumulation.
Typed provider-native tool status uses the generic `ToolUseState` renderer and
labels the row `<name> (native)`, with the qualifier visually subdued. A started
phase creates a live row and a completed phase settles it into current-session
history. These rows are transient UI projection: they do not enter Tau tool
state or counters and cannot be reconstructed after cold restart.

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
thinking, assistant response, provider compaction status, then active tool
summary/tool-call blocks. New live response-side blocks must be inserted before
active tool anchors rather than appended after them, so running tool UI remains
pinned nearest the prompt while assistant text continues streaming. Provider
compaction status trails the response so it remains visible when a long response
fills the viewport.

For an ordinary selected-transcript final, the CLI stages expensive terminal
projection for a canonical `provider.response_finished` before publishing it. It then retires the
live thinking, compaction, and response blocks; installs settled reasoning,
assistant, compaction, statistics, and tool-placeholder history; and publishes
editor and status state inside one redraw-suppressed transaction. A frame may
therefore contain the complete preceding live state or the complete settled final
state, but never a partial-final mixture. An already-started redraw may finish the
preceding complete snapshot.

A selected standalone-compaction final has no public provider projection to
stage. Its corresponding final transaction atomically retires the private live
prompt state and publishes any applicable content-free lifecycle state.

This cut is terminal-frame atomicity, not global serialization. It does not wait
for or synchronize tool starts, progress, or results, and it does not exclude
mutations from unrelated cloned terminal handles. Such output may join the
settled frame or a later frame. Hidden-transcript finalization stays off-screen
and requests no redraw.
