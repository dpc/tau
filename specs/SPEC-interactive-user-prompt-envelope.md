# SPEC-interactive-user-prompt-envelope: Interactive user prompt projection

## Record justification

Interactive prompt projection spans protocol provenance, transcript folding,
harness prompt assembly, provider-visible data, CLI history, compaction, and
replay, so no one local artifact can own the complete contract.

Every accepted prompt whose harness-stamped source is `HumanUi` projects into
provider context as exactly one user-role text item with the base form
`<user source="ui">{body}</user>`. The fixed `ui` value identifies the direct
Tau user-channel submission class; it does not attest a human author or a
particular CLI executable. The envelope has no added whitespace and uses
[SPEC-exact-sentinel-prompt-envelopes](SPEC-exact-sentinel-prompt-envelopes.md)
for attribute order and body framing.

The accepted raw text and typed source remain canonical in submitted or steered
prompt facts. UI requests, transcript presentation, history, navigation, watch
fanout, and compaction replacement windows remain raw. Typed provenance travels
through the derived transcript, and only provider assembly renders the envelope.
Live assembly and replay use the same typed renderer and never infer source.

The envelope adds `delayed="true"` only when a user-visible assistant response
completed after submission but before the queued input's accepted prompt fact.
The harness captures the response baseline at validated target-bound UI admission,
before interaction publication can wait. Commentary counts at canonical response
completion, including retained text in canceled responses; partial streaming,
reasoning, tools, status, private compaction, and waiting alone do not count.
Input submitted during streaming is delayed if that response completes first.
The accepted decision travels with submitted and steered facts through replay
and surviving typed compaction suffixes. Runtime queues remain non-durable.
Absent or false historical metadata leaves the envelope unmarked; the renderer
does not infer delay from adjacency.

The shared model instruction says: `delayed="true"` means the user prompt was
submitted before the agent’s latest response was complete. Do not assume it
replies to that response. This is causal context, not a read receipt or response
identifier. Other input families do not acquire this attribute.

Already materialized compaction and provider-native replay bytes remain
unchanged. Historical fieldless `<user>...</user>` bytes are recognized only
to include the model-visible family notice, never to infer or reproject source.

This covers existing-agent prompts, new-agent initial prompts, queued or steered
interactive prompts, and successful `:skill` expansion. Internal, injected,
extension, external-message, agent-message, and watch inputs retain their
separate projections. The envelope creates no second payload fact, activation,
wake, route, identity, or trust authority.
