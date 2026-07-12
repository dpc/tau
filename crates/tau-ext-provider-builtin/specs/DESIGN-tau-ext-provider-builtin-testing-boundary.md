# DESIGN-tau-ext-provider-builtin-testing-boundary: This crate tests registry/runtime integration, not backend protocol matrices

Status: inferred

This crate's tests cover provider profile serialization, CLI behavior, model
publication/routing, runtime event ordering, cancellation/retry bookkeeping, and
final provider event shapes. Backend wire-format parsing and HTTP/SSE/WebSocket
transport details belong in `tau-provider-chatgpt` and
`tau-provider-chat-completions`; this crate should test its integration with
those backends without duplicating their protocol parser matrices.
