# ARCH-tau-ext-websearch: tau-ext-websearch architecture

`std-websearch` / `tau-ext-websearch` is enabled by default and sends model tool
arguments to external hosted MCP web providers. Treat provider responses as
untrusted web content that can contain prompt injection, misleading text, or large
payloads. The extension must keep successful response bodies, decoded
model-visible output, and concurrency bounded.

Endpoint override URLs are configuration but may still contain secrets in
userinfo, query strings, or fragments. The extension must not log raw endpoint
override URLs, must reject URL userinfo credentials and unsupported auth forms,
and must not send Parallel Authorization headers. Production provider endpoints
must use HTTPS; plaintext HTTP is only acceptable for loopback test endpoints.
Provider transport diagnostics and JSON-RPC errors can be surfaced as
model-visible tool errors, so configured endpoint echoes, request targets, query
keys/values, fragments, and userinfo must be sanitized and finally bounded before
return.
