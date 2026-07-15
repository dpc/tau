# DESIGN-tau-ext-provider-builtin-testing-boundary: This crate tests registry/runtime integration, not backend protocol matrices

Status: inferred

This crate's tests cover provider profile serialization, CLI behavior, model
publication/routing, runtime event ordering, cancellation/retry bookkeeping, and
final provider event shapes. Backend wire-format parsing and HTTP/SSE/WebSocket
transport details belong in `tau-provider-chatgpt` and
`tau-provider-chat-completions`; this crate should test its integration with
those backends without duplicating their protocol parser matrices.

Shared OAuth body caps, flat/nested error-envelope parsing, bounded untrusted
fields, and credential-safe default formatting belong to `tau-provider`.
Changes in this crate that log typed OAuth failures should add an integration
regression proving the consumer uses only that safe projection while preserving
provider attribution.

OAuth refresh integration tests use temporary auth files and injected endpoint
outcomes. They cover exact-generation suppression, credential/mode invalidation,
authoritative locked-generation handoff, expired-versus-valid fallback, and
credential-safe provider-attributed warnings without live auth, Internet, or
wall-clock sleeps.
