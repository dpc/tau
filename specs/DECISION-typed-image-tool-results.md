# DECISION-typed-image-tool-results: Native typed image tool results

Authority: confirmed, 2026-07-14, dpc

Local image inspection produces native typed tool-result content rather than
base64 text or synthesized user messages. The function call retains causal
identity and normalized text while canonical normalized image bytes remain the
durable replay and inference authority.

Only explicitly audited provider/model routes publishing image-input and
image-tool-result capabilities receive bytes. Generic UI, event subscriber,
debug, and diagnostic projections omit them.

This keeps media in the tool-result lifecycle without granting unaudited routes
or observers payload access. Exact behavior is specified by
[SPEC-typed-image-tool-results](SPEC-typed-image-tool-results.md).
