# SPEC-tau-ext-websearch-provider-boundary: Hosted provider boundary

## Record justification

The provider boundary spans adapter-specific tool schemas, endpoint
configuration, MCP/REST transport, result projection, and diagnostic sanitization, so
no single implementation area can own the complete external-content contract.

Provider calls send model-supplied tool arguments to external hosted MCP or
REST services. Provider output is untrusted web content that can contain prompt
injection, misleading text, or large payloads before it re-enters model context.
For prompt-correlated calls, the harness may attach a model-hidden
`allowed_web_domains` invocation policy to `tool.started`. The extension never
accepts this authority from model-visible arguments or `tool.request`.

Fetch validates and normalizes the requested absolute HTTP(S) target before
permit acquisition, provider selection, or extractor contact. It rejects
userinfo, IP literals, and hosts outside the exact-or-subdomain allowlist. This
is a requested-target gate, not a network sandbox: an extractor can follow
redirects or load subresources outside the allowlist.

Restricted search is eligible only for adapters whose declaration promises
provider-side per-call filtering. Filtering returned results is not an egress
control. When no configured adapter declares enforcement, the logical search
candidate is unavailable and performs zero network activity.
Each You.com attempt performs the MCP initialize/initialized
handshake before `tools/call`, sends the negotiated protocol header only after
initialization, requires the server's tools capability, and returns any
server-issued session id on subsequent requests. All handshake requests share
the scheduler-owned attempt deadline and, when configured, the same bearer
credential.

Every successful result has exactly one extension-owned projection:
`<tau_web_content adapter="exa|parallel|you|brave|tavily|firecrawl" operation="search|fetch"
content_trust="external">…</tau_web_content>`. Attribute values are closed:
Exa, Parallel, Tavily, and Firecrawl support search and fetch. You.com and
Brave support search only. Attribute order is
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

Endpoint URLs are validated when configuration is applied. Production endpoints
must use HTTPS; plaintext HTTP is accepted only for loopback test endpoints.
Userinfo credentials and unsupported schemes are rejected. Provider requests do
not follow HTTP redirects because a redirect target has not passed this endpoint
validation and could cross the configured scheme, origin, and redaction
boundary. A provider that redirects must be configured using its final URL.

Raw endpoint URLs are log-sensitive because userinfo, query strings, or
fragments can contain secrets, so logs do not print them. Provider transport
diagnostics and JSON-RPC error messages can become model-visible tool errors.
Before return, endpoint-derived secrets are sanitized and the diagnostic is
bounded.

Credentialed adapters resolve API keys from named Tau secrets rather than
ordinary extension configuration. Provider requests carry credentials only in
the documented authentication header. Model-visible and logged diagnostics
redact both endpoint material and credential values.

Response and diagnostic bounds are specified by
[SPEC-tau-ext-websearch-runtime-safeguards](SPEC-tau-ext-websearch-runtime-safeguards.md).
Provider defaults, configuration aliases, and tool shape are described by
[the component README](../README.md) and
[ARCH-tau-ext-websearch](ARCH-tau-ext-websearch.md).

Composite calls return only the first non-empty successful provider projection;
they do not merge, rank, or deduplicate provider bodies. All-provider errors
contain only ordered stable adapter/category pairs and are bounded to 1 KiB.
Compact attempt display uses only closed adapter names and markers, never raw
errors, endpoints, credentials, queries, or requested URLs.
