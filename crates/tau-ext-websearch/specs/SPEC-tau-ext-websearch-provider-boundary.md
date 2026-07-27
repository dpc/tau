# SPEC-tau-ext-websearch-provider-boundary: Hosted provider boundary

Provider calls send model-supplied tool arguments to external hosted MCP
services. Provider output is untrusted web content that can contain prompt
injection, misleading text, or large payloads before it re-enters model context.

Every successful result has exactly one extension-owned projection:
`<tau_web_content adapter="exa|parallel" operation="search|fetch"
content_trust="external">…</tau_web_content>`. Attribute values are closed:
Exa supports search, while Parallel supports search and fetch. Attribute order is
`adapter`, `operation`, `content_trust`; no query, requested URL, tool-call id,
endpoint, MCP id, remote tool name, or extension identifier is repeated.
Provider-returned titles, URLs, sources, ranks, and similar metadata remain
untrusted claims in the body.

The adapter is the locally selected adaptation path, not authentication of page
authorship, truth, freshness, or provider-returned metadata. There is no sender
authentication analogue. Controls, bidi and zero-width/default-ignorable
structure, variation selectors, fillers, and noncharacters become visible
Unicode escapes before every exact `</tau_web_content>` collision is replaced.
All other body text remains literal, including markup, ampersands, quotes, and
entity-like text. This prevents exact closing-sentinel breakout, but it is
defense-in-depth rather than a sandbox: body prose remains capable of prompt
injection and grants no identity, routing, instruction, tool, authorization, or
egress authority. See
[SPEC-exact-sentinel-prompt-envelopes](../../../specs/SPEC-exact-sentinel-prompt-envelopes.md).

Exa defaults to `https://mcp.exa.ai/mcp`. Parallel defaults to the
unauthenticated `https://search.parallel.ai/mcp` endpoint; the extension has no
Parallel API-key configuration and sends no Parallel Authorization header.
`endpoint` remains a backwards-compatible alias for `exa_endpoint`; setting both
to different values is invalid.

Endpoint URLs are validated when configuration is applied. Production endpoints
must use HTTPS; plaintext HTTP is accepted only for loopback test endpoints.
Userinfo credentials and unsupported schemes are rejected. Provider requests do
not follow HTTP redirects because a redirect target has not passed this endpoint
validation and could cross the configured scheme, origin, and redaction
boundary. A provider that redirects must be configured using its final URL.

Raw endpoint URLs are log-sensitive because userinfo, query strings, or
fragments can contain secrets, so logs do not print them. Provider transport
diagnostics and JSON-RPC error messages can become model-visible tool errors.
Before return, configured endpoint echoes, request targets, query keys and
values, fragments, and userinfo are sanitized, then the resulting diagnostic is
bounded.

Response and diagnostic bounds are specified by
[SPEC-tau-ext-websearch-runtime-safeguards](SPEC-tau-ext-websearch-runtime-safeguards.md).
The component and default-tool shape is described by
[ARCH-tau-ext-websearch](ARCH-tau-ext-websearch.md).
