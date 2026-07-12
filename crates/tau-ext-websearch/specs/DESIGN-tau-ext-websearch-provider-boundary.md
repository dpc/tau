# DESIGN-tau-ext-websearch-provider-boundary: Provider boundary

Status: unconfirmed

`tau-ext-websearch` adapts hosted MCP web providers into Tau tools. Exa search is
enabled by default; Parallel search/fetch tools are registered but disabled by
default for explicit role opt-in. Provider output is untrusted model input.

Endpoint fields are ordinary extension config, but raw endpoint URL values are
log-sensitive because URLs can contain credentials in userinfo, query strings, or
fragments. Endpoint URLs are validated before use, userinfo is rejected, and
HTTPS is required except for loopback HTTP endpoints used by deterministic tests.
Logs must not print raw endpoint URLs. Provider transport diagnostics and
JSON-RPC error messages may become model-visible tool errors, so they must be
sanitized for configured endpoint echoes, request targets, query keys/values,
fragments, and userinfo before being returned.
