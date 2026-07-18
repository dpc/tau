# SPEC-tau-provider-chatgpt-curated-vcr: Curated provider compatibility corpus

`tau-provider-chatgpt` owns the public cassette corpus, manifest, audit,
production-parser compatibility assertions, and refresh review. `tau-vcr` owns only
schema-agnostic bounded storage and mode handling.

Each structurally allowlisted synthetic cassette represents one successful
request/stream attempt, requires exact frame and terminal consumption, and ignores
recorded delay during functional replay.

Public refreshes add a new key without overwriting reviewed evidence, record
synthetic provenance and compatibility intent in the manifest, pass the
network-denied replay-only audit, and receive independent privacy review.

Implements
[`DECISION-tau-provider-chatgpt-curated-vcr`](DECISION-tau-provider-chatgpt-curated-vcr.md).
