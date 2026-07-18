# DECISION-typed-image-tool-results: Native typed image tool results

Authority: confirmed, 2026-07-14, dpc

Local image inspection produces native typed tool-result content rather than
base64 text or synthesized user messages. The original function call retains its
causal identity and normalized text result while canonical normalized image bytes
remain the durable replay and inference authority.

Only explicitly audited provider/model routes that publish both image-input and
image-tool-result capabilities receive image bytes. Generic UI, event subscriber,
debug, and diagnostic projections omit those bytes. Image-producing tools remain
foreground-only while background completion events cannot carry typed provider
content.

This keeps media within the existing tool-call/result lifecycle and retention
model without granting unaudited routes or generic observers access to payload
bytes. Formats, normalization profiles, resource and history limits, metadata,
provider lowering, replay, compaction, and projection behavior are specified by
[SPEC-typed-image-tool-results](SPEC-typed-image-tool-results.md).
