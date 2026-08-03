# tau-ext-websearch

A Tau extension that registers generic web search tools. The existing Exa-backed
search remains enabled by default, and Parallel.ai search/fetch tools are
registered in the same extension but disabled by default for role-level opt-in.

## Tools

- `websearch_exa`, advertised to models as `web_search`, proxies Exa's keyless
  hosted MCP at <https://mcp.exa.ai/mcp>.
- `websearch_parallel_search`, advertised to models as `web_search`, proxies the
  default unauthenticated Parallel Search MCP endpoint at
  <https://search.parallel.ai/mcp>. This tool is disabled by default to avoid a
  duplicate model-visible `web_search` unless a role explicitly enables it.
  JSON-convertible provider-specific arguments are passed through to Parallel in
  addition to the documented `query` field.
- `websearch_parallel_fetch`, advertised to models as `web_fetch`, fetches and
  extracts a page through the same unauthenticated Parallel MCP endpoint. This
  tool is also disabled by default. JSON-convertible provider-specific arguments
  are passed through in addition to the documented `url` field.

No Parallel API key is supported: there is no `api_key` config, and the
extension does not send an Authorization header.

## Configuration

The built-in extension is `std-websearch` and is enabled by default. Disable it
if you'd rather not make outbound HTTP calls:

```json5
{
  extensions: {
    "std-websearch": { enable: false },
  },
}
```

Endpoint overrides:

```json5
{
  extensions: {
    "std-websearch": {
      config: {
        // Backwards-compatible alias for exa_endpoint.
        endpoint: "https://mcp.exa.ai/mcp?exaApiKey=sk-…",
        // Or explicitly:
        exa_endpoint: "https://mcp.exa.ai/mcp",
        parallel_endpoint: "https://search.parallel.ai/mcp",
      },
    },
  },
}
```

If both `endpoint` and `exa_endpoint` are set, they must have the same value.
Endpoint values are validated when configuration is applied. They must be HTTPS
URLs, except that HTTP loopback URLs are accepted for deterministic local tests.
Endpoint userinfo credentials are rejected, and logs intentionally avoid
printing raw endpoint URLs because they can contain credentials in userinfo,
query strings, or fragments. Provider requests do not follow HTTP redirects:
configure the final endpoint URL directly when a provider publishes a redirect.

## Runtime and security assumptions

The extension makes outbound HTTPS requests to the configured MCP providers and
treats all returned text and metadata as untrusted external web data. Every
successful result is an ordinary tool-result string enclosed in
`<tau_web_content adapter="exa|parallel" operation="search|fetch"
content_trust="external">…</tau_web_content>`. Tau owns the closed outer
attributes, makes structural Unicode visible, and replaces exact outer-close
collisions while preserving all other provider text. The adapter label identifies only the configured
adaptation path; it does not authenticate a page author, URL, title, rank,
freshness, or truth. The boundary is defense-in-depth, not a prompt-injection
sandbox or a grant of instruction authority.

The extension caps HTTP error bodies, successful MCP response bodies, decoded
provider text, and the final framed result so provider responses cannot
grow without bound. A projected result over 512 KiB fails rather than being
silently truncated. At most eight searches/fetches run at once; additional calls
fail fast with a busy tool error so harness control messages are not blocked
behind network calls.

Provider transport diagnostics and JSON-RPC error messages can become
model-visible `ToolError` text. Before surfacing them, the extension sanitizes
echoes of the configured endpoint URL, request target, query keys/values,
fragment, and userinfo, then applies a final model-visible error cap. Oversized
JSON-RPC error messages are replaced with a compact deterministic error instead
of echoing provider text. HTTP 429 responses instead produce the generic bounded
advice `web service rate-limited the request; try again later.` without reading
or echoing the provider body.

Tests use local stubs or loopback HTTP servers and do not contact live providers.

## Tracing

```sh
TAU_LOG=websearch=debug tau …
```
