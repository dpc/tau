# SPEC-interactive-user-prompt-envelope: Interactive user prompt projection

## Record justification

Interactive prompt projection spans protocol provenance, transcript folding,
harness prompt assembly, provider-visible data, CLI history, compaction, and
replay, so no one local artifact can own the complete contract.

Every accepted prompt whose harness-stamped source is `HumanUi` projects into
provider context as exactly one user-role text item:
`<user source="ui">{body}</user>`. The fixed `ui` value identifies the direct
Tau user-channel submission class; it does not attest a human author or a
particular CLI executable. The envelope has no added whitespace and uses
[SPEC-exact-sentinel-prompt-envelopes](SPEC-exact-sentinel-prompt-envelopes.md)
for attribute order and body framing.

The accepted raw text and typed source remain canonical in submitted or steered
prompt facts. UI requests, transcript presentation, history, navigation, watch
fanout, and compaction replacement windows remain raw. Typed provenance travels
through the derived transcript, and only provider assembly renders the envelope.
Live and replay use the same typed renderer; source is never inferred.
Already materialized compaction and provider-native replay bytes remain
unchanged. Historical fieldless `<user>...</user>` bytes are recognized only
to include the model-visible family notice, never to infer or reproject source.

This covers existing-agent prompts, new-agent initial prompts, queued or steered
interactive prompts, and successful `:skill` expansion. Internal, injected,
extension, external-message, agent-message, and watch inputs retain their
separate projections. The envelope creates no second payload fact, activation,
wake, route, identity, or trust authority.
