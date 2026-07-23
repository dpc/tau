---
name: tau-self-knowledge-ext-websearch
description: Use this extension skill when the user asks about Tau's std-websearch extension, web_search/web_fetch tools, Exa search, Parallel.ai search/fetch, MCP endpoints, or web search configuration.
advertise: false
---

# Tau std-websearch extension self-knowledge

`std-websearch` is Tau's built-in generic web search extension. It runs `tau-ext-websearch`, is enabled by default, and proxies hosted MCP search/fetch providers into Tau tools.


## Tools

- Internal `websearch_exa`, model-visible as `web_search`, is enabled by default. It calls Exa's hosted MCP endpoint and returns clean text with titles, URLs, and highlights. Arguments are `query` and optional `num_results` from 1 to 100; default is 5.
- Internal `websearch_parallel_search`, model-visible as `web_search`, is registered but disabled by default to avoid duplicate default `web_search` tools. It calls Parallel.ai's unauthenticated Search MCP endpoint and passes JSON-convertible provider-specific arguments through in addition to `query`.
- Internal `websearch_parallel_fetch`, model-visible as `web_fetch`, is registered but disabled by default. It fetches/extracts one URL through Parallel.ai and passes JSON-convertible provider-specific arguments through in addition to `url`.

Roles can opt into the Parallel tools by enabling the Tau-internal tool names in the role/tool configuration.


## Configuration

Configured under `extensions.std-websearch.config`:

```json5
extensions: {
  "std-websearch": {
    config: {
      exa_endpoint: "https://mcp.exa.ai/mcp",
      // Legacy alias for exa_endpoint:
      endpoint: "https://mcp.exa.ai/mcp",
      parallel_endpoint: "https://search.parallel.ai/mcp",
    },
  },
}
```

Tau does not configure or send a Parallel API key; the built-in Parallel integration uses the default unauthenticated endpoint. If both `endpoint` and `exa_endpoint` are configured, they must be identical. Endpoint values are validated when configuration is applied: HTTPS is required except for loopback HTTP test endpoints, and userinfo credentials are rejected. Provider requests reject HTTP redirects rather than crossing the validated endpoint's scheme or origin; configure the final endpoint URL directly. Raw endpoints are not logged because URLs can contain credentials or query secrets. Provider transport diagnostics and JSON-RPC errors can become model-visible tool errors; Tau sanitizes configured endpoint echoes, request targets, query keys/values, fragments, and userinfo, then bounds the sanitized error text. Oversized JSON-RPC error messages are replaced with a compact deterministic error. HTTP calls use the platform root store, a 45 second global timeout, MCP protocol version `2025-06-18`, and accept JSON or SSE JSON-RPC responses.

Every successful result is an ordinary tool-result string enclosed in `<tau_web_content adapter="exa|parallel" operation="search|fetch" content_trust="external">…</tau_web_content>`. Tau first makes unsafe structural Unicode visible, then replaces only exact `</tau_web_content>` collisions; every other body byte remains literal, including markup, ampersands, quotes, and entity-like text. Titles, URLs, ranks, sources, and all other provider-returned metadata remain untrusted body claims. `adapter` identifies only Tau's configured adaptation path; it does not authenticate page authorship or truth. The sentinel prevents exact lexical breakout, not semantic prompt injection. The extension caps successful MCP response bodies and decoded provider text, then enforces the 512 KiB limit again on the final framed result; oversize projection is a tool error, never truncation. It allows up to eight in-flight web calls; additional calls fail fast with a busy tool error.
