# DESIGN-tau-provider-chatgpt-curated-vcr: Curated VCR is successful-stream compatibility evidence

Status: unconfirmed

`tau-provider-chatgpt` owns the public provider cassette corpus, its manifest
and audit, production-parser compatibility facts, and refresh review.
`tau-vcr` owns only schema-agnostic bounded storage and mode handling.

The public corpus is synthetic and structurally allowlisted. It represents one
successful request/stream attempt per cassette, requires exact frame and
terminal consumption, and ignores recorded delay during functional replay.
It is not authority for scheduler behavior, retries, concurrency, provider
prose, or timing.

An attempt-sequence/AP-lane schema is intentionally deferred. It would
duplicate the deterministic scheduler and local scripted-transport gates while
creating another evolving semantic format. Reconsider only when a concrete
provider compatibility regression cannot be represented by successful stream
evidence or the local transport fixtures. Any reconsideration requires a new,
explicitly versioned full schema rather than extending the success-stream
format into scheduler authority.

Public refreshes add a new key without overwriting reviewed evidence, state
synthetic provenance and compatibility intent in the manifest, pass the
network-denied replay-only audit, and receive independent privacy review.
